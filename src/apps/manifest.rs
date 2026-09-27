//! The parts of `senclaw-manifest.json` that decide **how** a Space App runs.
//!
//! The manifest is handled as a raw `serde_json::Value` everywhere else, because
//! most of it is passed through to the Web UI untouched. The things the daemon
//! must actually *decide* on are typed here, in one place, so a missing or
//! misspelled field has exactly one meaning:
//!
//! - [`RunMode`] — background (always on) or session (on demand). Default:
//!   session, because most apps are a screen the user opens, and 50 idle
//!   servers is what the old always-on default actually cost.
//! - [`Runner`] — what kind of program `runtime.start` is (a native binary, a
//!   Node app, a Python app, a shell line). Decides which interpreter must
//!   exist and whether dependencies need installing before the first launch.
//! - [`Requires`] — what the *machine* must provide: a Node version, a Python
//!   version, `ffmpeg` on `PATH`. Checked at install so the failure is a
//!   sentence at install time instead of a stack trace in a log file weeks later.
//! - [`SandboxDecl`] — the confinement the app itself asks for, which the
//!   install applies, and which `force` makes non-negotiable.
//! - [`LlmDecl`] — whether the app serves models SenClaw can route turns to.
//!   The one parser here that returns an error instead of a default; see its
//!   docs for why this block's failures are not survivable.
//!
//! Everything is optional. An app that declares none of it behaves exactly as
//! Space Apps did before this module existed, except that it is now a *session*
//! app.

use serde_json::Value;

/// How the daemon keeps an app's server process alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Started when SenClaw starts, supervised, restarted when it dies. For
    /// apps that do work nobody asked for at that moment: polling a channel for
    /// inbound messages, running a schedule, holding the WebSocket a browser
    /// extension dials into.
    Background,
    /// Started on demand — when the user opens the app, or when an agent calls
    /// one of its MCP tools — and stopped again once it has been idle for
    /// [`RuntimeSpec::idle_timeout_secs`].
    Session,
}

impl Default for RunMode {
    fn default() -> Self {
        RunMode::Session
    }
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RunMode::Background => "background",
            RunMode::Session => "session",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            // `always` / `daemon` / `resident` are what people write when they
            // mean background; accepting them costs nothing and a silently
            // ignored mode is the worst outcome here.
            "background" | "always" | "daemon" | "resident" => Some(RunMode::Background),
            "session" | "ondemand" | "on-demand" | "lazy" => Some(RunMode::Session),
            _ => None,
        }
    }

    pub fn is_background(self) -> bool {
        matches!(self, RunMode::Background)
    }
}

/// How long a session app may sit unused before it is stopped.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 60;

/// Never below this: a shorter window turns a user reading one screen into a
/// stop/start loop, and restarting costs more than the idle process saved.
pub const MIN_IDLE_TIMEOUT_SECS: u64 = 15;

/// What kind of program `runtime.start` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// A native executable shipped with the app (`./crm`). No interpreter, no
    /// dependency install — this is what almost every bundled app is.
    Binary,
    /// A Node program. Needs `node` on `PATH`, and `node_modules` present
    /// before the first launch.
    Node,
    /// A Python program. Needs an interpreter, and — when the app ships
    /// `requirements.txt` or declares `runtime.install` — its own virtualenv,
    /// so installing the app's dependencies never touches the system Python.
    Python,
    /// Anything else: run the line through the platform shell and hope the app
    /// knows what it is doing. The historical behaviour.
    Shell,
}

impl Runner {
    pub fn as_str(self) -> &'static str {
        match self {
            Runner::Binary => "binary",
            Runner::Node => "node",
            Runner::Python => "python",
            Runner::Shell => "shell",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "binary" | "native" | "exe" => Some(Runner::Binary),
            "node" | "nodejs" | "npm" | "js" | "javascript" | "typescript" => Some(Runner::Node),
            "python" | "python3" | "py" => Some(Runner::Python),
            "shell" | "sh" | "bash" => Some(Runner::Shell),
            _ => None,
        }
    }

    /// Guess from the start command when the manifest does not say. Only the
    /// unambiguous cases are claimed; everything else stays `Shell`, which
    /// behaves exactly as before.
    pub fn infer(start: &str) -> Runner {
        let s = start.trim();
        let first = s.split_whitespace().next().unwrap_or("");
        let base = first.rsplit(['/', '\\']).next().unwrap_or(first);
        match base {
            "node" | "npm" | "npx" | "pnpm" | "yarn" | "bun" | "deno" => Runner::Node,
            "python" | "python3" | "py" | "uv" | "poetry" | "pipenv" => Runner::Python,
            _ if base.ends_with(".js") || base.ends_with(".mjs") || base.ends_with(".cjs") => {
                Runner::Node
            }
            _ if base.ends_with(".py") => Runner::Python,
            // `./crm`, `./deepwiki` — a program shipped in the app directory.
            _ if s.starts_with("./") || s.starts_with(".\\") => Runner::Binary,
            _ => Runner::Shell,
        }
    }
}

/// The `runtime` block, as far as the daemon is concerned.
#[derive(Debug, Clone)]
pub struct RuntimeSpec {
    /// `runtime.kind == "server"` **and** a `start` command — the only shape
    /// the daemon launches anything for.
    pub is_server: bool,
    pub start: Option<String>,
    pub port: u16,
    pub health_path: String,
    pub mode: RunMode,
    pub idle_timeout_secs: u64,
    pub runner: Runner,
    /// One-off command run in the app directory before the first launch after
    /// an install or update (`npm ci`, `pip install -r requirements.txt`).
    pub install: Option<String>,
    /// Give a Python app its own `.venv`. Defaults to true for `Runner::Python`
    /// — installing an app's dependencies into the user's system Python is not
    /// ours to do.
    pub venv: bool,
}

impl Default for RuntimeSpec {
    fn default() -> Self {
        RuntimeSpec {
            is_server: false,
            start: None,
            port: 0,
            health_path: "/health".to_string(),
            mode: RunMode::default(),
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            runner: Runner::Shell,
            install: None,
            venv: false,
        }
    }
}

impl RuntimeSpec {
    pub fn parse(manifest: &Value) -> RuntimeSpec {
        let Some(rt) = manifest.get("runtime") else {
            return RuntimeSpec::default();
        };
        let start = rt
            .get("start")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        let is_server =
            rt.get("kind").and_then(Value::as_str) == Some("server") && start.is_some();

        let runner = rt
            .get("runner")
            .and_then(Value::as_str)
            .and_then(Runner::parse)
            .unwrap_or_else(|| Runner::infer(start.as_deref().unwrap_or("")));

        let mode = rt
            .get("mode")
            .and_then(Value::as_str)
            .and_then(RunMode::parse)
            .unwrap_or_default();

        let idle_timeout_secs = rt
            .get("idleTimeoutSecs")
            .or_else(|| rt.get("idle_timeout_secs"))
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS)
            .max(MIN_IDLE_TIMEOUT_SECS);

        RuntimeSpec {
            is_server,
            port: rt.get("port").and_then(Value::as_u64).unwrap_or(0) as u16,
            health_path: rt
                .get("healthPath")
                .and_then(Value::as_str)
                .unwrap_or("/health")
                .to_string(),
            mode,
            idle_timeout_secs,
            runner,
            install: rt
                .get("install")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty()),
            venv: rt
                .get("venv")
                .and_then(Value::as_bool)
                .unwrap_or(matches!(runner, Runner::Python)),
            start,
        }
    }
}

/// One thing the machine must provide.
#[derive(Debug, Clone, PartialEq)]
pub struct Requirement {
    /// `node`, `python`, or the name of an executable that must be on `PATH`.
    pub name: String,
    pub kind: RequirementKind,
    /// A version range, when one was asked for: `>=18`, `>=3.10 <4`, `18.x`.
    pub range: Option<String>,
    /// A missing optional requirement is reported and does not stop anything.
    pub optional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequirementKind {
    /// A language runtime whose version is read with `--version`.
    Node,
    Python,
    /// Any executable that must simply exist on `PATH`.
    Bin,
    /// An environment variable that must be set and non-empty.
    Env,
}

impl RequirementKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RequirementKind::Node => "node",
            RequirementKind::Python => "python",
            RequirementKind::Bin => "bin",
            RequirementKind::Env => "env",
        }
    }
}

/// The `requires` block:
///
/// ```json
/// "requires": {
///   "node": ">=18",
///   "python": ">=3.10",
///   "bin": ["ffmpeg", "git"],
///   "optionalBin": ["yt-dlp"],
///   "env": ["OPENAI_API_KEY"],
///   "os": ["macos", "linux"]
/// }
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Requires {
    pub items: Vec<Requirement>,
    /// Platforms the app supports (`macos` / `linux` / `windows`). Empty means
    /// "all".
    pub os: Vec<String>,
}

impl Requires {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.os.is_empty()
    }

    pub fn parse(manifest: &Value) -> Requires {
        let Some(req) = manifest.get("requires").or_else(|| manifest.get("requirements")) else {
            return Requires::default();
        };
        let mut items = Vec::new();

        // `"node": ">=18"` — or `"node": {"version": ">=18", "optional": true}`.
        for (key, kind) in [("node", RequirementKind::Node), ("python", RequirementKind::Python)] {
            if let Some(v) = req.get(key) {
                let (range, optional) = match v {
                    Value::String(s) => (Some(s.clone()), false),
                    Value::Object(o) => (
                        o.get("version")
                            .or_else(|| o.get("range"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        o.get("optional").and_then(Value::as_bool).unwrap_or(false),
                    ),
                    Value::Bool(true) => (None, false),
                    _ => continue,
                };
                items.push(Requirement {
                    name: key.to_string(),
                    kind,
                    range: range.filter(|r| !r.trim().is_empty() && r.trim() != "*"),
                    optional,
                });
            }
        }

        let mut push_list = |key: &str, kind: RequirementKind, optional: bool| {
            if let Some(arr) = req.get(key).and_then(Value::as_array) {
                for entry in arr {
                    // `"ffmpeg"` or `{"name": "ffmpeg", "optional": true}`.
                    let (name, opt) = match entry {
                        Value::String(s) => (s.trim().to_string(), optional),
                        Value::Object(o) => (
                            o.get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .trim()
                                .to_string(),
                            o.get("optional").and_then(Value::as_bool).unwrap_or(optional),
                        ),
                        _ => continue,
                    };
                    if name.is_empty() {
                        continue;
                    }
                    items.push(Requirement { name, kind, range: None, optional: opt });
                }
            }
        };
        push_list("bin", RequirementKind::Bin, false);
        push_list("optionalBin", RequirementKind::Bin, true);
        push_list("env", RequirementKind::Env, false);
        push_list("optionalEnv", RequirementKind::Env, true);

        let os = req
            .get("os")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        Requires { items, os }
    }
}

/// An app that serves models, declared in the manifest's `llm` block.
///
/// ```json
/// "llm": {
///   "autoRegister": true,
///   "path": "/v1",
///   "adapt": "openai",
///   "displayName": "MLX (Apple Silicon)"
/// }
/// ```
///
/// One block per app, not a list: an app registers one MCP server, and giving
/// it several LLM endpoints instead would add an axis of failure for a case
/// nothing needs. Which *models* it serves is a separate question, answered by
/// the app's own `/v1/models` (and, while it is stopped, by the cache it wrote
/// at startup) — the manifest never names them, so adding a model is not a
/// reinstall.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmDecl {
    /// Register the provider automatically at install and at boot. `false`
    /// parses fine and does nothing — the app is then only reachable by
    /// whatever configures it by hand.
    pub auto_register: bool,
    /// Path prefix the OpenAI surface is mounted at, without a trailing slash.
    pub path: String,
    /// Wire format, from [`APP_DECLARABLE_ADAPTERS`].
    pub adapt: String,
    /// Label for the model picker. Falls back to the app's own name.
    pub display_name: Option<String>,
    /// `editFormat` for every model the app serves (`exact` | `fuzzy` |
    /// `udiff` | `whole`). A local checkpoint usually wants `fuzzy` or
    /// `udiff`; absent = exact.
    pub edit_format: Option<crate::zen_core::EditFormat>,
}

/// Wire formats a Space App may declare. Mirrors
/// [`crate::zen_core::query_llm::APP_DECLARABLE_ADAPTERS`], which is the
/// authority; the test below pins them together.
use crate::zen_core::query_llm::APP_DECLARABLE_ADAPTERS;

impl Default for LlmDecl {
    fn default() -> Self {
        LlmDecl {
            auto_register: false,
            path: "/v1".to_string(),
            adapt: "openai".to_string(),
            display_name: None,
            edit_format: None,
        }
    }
}

impl LlmDecl {
    /// `Ok(None)` when the app declares no `llm` block at all.
    ///
    /// Everything else in this module falls back to a default when a field is
    /// missing or misspelled, because the failure is recoverable — a session app
    /// that meant to be `background` still runs. This one returns `Err` instead,
    /// and the install refuses the app. The reason is that the failures here are
    /// not recoverable and not visible: an `adapt` the daemon does not route
    /// means every turn is sent an OpenAI body and fails somewhere upstream with
    /// a parse error naming neither the app nor the field, and `adapt:
    /// "local-mlx"` means the turn never reaches the app's port at all — it goes
    /// to an in-process engine, and the app sits registered and permanently
    /// unused. Both read as "the model is broken", months after the typo.
    pub fn parse(manifest: &Value) -> Result<Option<LlmDecl>, String> {
        let Some(llm) = manifest.get("llm") else {
            return Ok(None);
        };
        if !llm.is_object() {
            return Err("`llm` must be an object".to_string());
        }

        let adapt = match llm.get("adapt") {
            None => "openai".to_string(),
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| "`llm.adapt` must be a string".to_string())?
                    .trim()
                    .to_ascii_lowercase();
                if !APP_DECLARABLE_ADAPTERS.contains(&s.as_str()) {
                    return Err(format!(
                        "`llm.adapt` is `{s}`; a Space App may declare only {}",
                        APP_DECLARABLE_ADAPTERS.join(" or ")
                    ));
                }
                s
            }
        };

        let path = match llm.get("path") {
            None => "/v1".to_string(),
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| "`llm.path` must be a string".to_string())?
                    .trim()
                    .trim_end_matches('/')
                    .to_string();
                if s.is_empty() {
                    // The daemon appends `/chat/completions`; mounting at the
                    // app's root would collide with its own UI routes.
                    return Err("`llm.path` must not be empty (default is `/v1`)".to_string());
                }
                if !s.starts_with('/') {
                    return Err(format!("`llm.path` must start with `/`, got `{s}`"));
                }
                s
            }
        };

        // A non-bool here is a typo the app author meant as `true`
        // (`"autoRegister": "true"`), and reading it as `false` would leave the
        // provider silently unregistered — the exact class of failure this
        // function exists to refuse.
        let auto_register = match llm.get("autoRegister") {
            None => false,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| "`llm.autoRegister` must be true or false".to_string())?,
        };

        let display_name = match llm.get("displayName") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .ok_or_else(|| "`llm.displayName` must be a string".to_string())?
                    .trim()
                    .to_string(),
            )
            .filter(|s: &String| !s.is_empty()),
        };

        // Like `adapt`, a misspelled value is an error rather than a silent
        // default: `"fuzy"` would leave a local model on exact matching, which
        // is exactly the failure this field exists to prevent.
        let edit_format = match llm.get("editFormat") {
            None => None,
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| "`llm.editFormat` must be a string".to_string())?;
                Some(crate::zen_core::EditFormat::parse(s).ok_or_else(|| {
                    format!("`llm.editFormat` must be one of exact, fuzzy, udiff, whole (got {s:?})")
                })?)
            }
        };

        Ok(Some(LlmDecl {
            auto_register,
            path,
            adapt,
            display_name,
            edit_format,
        }))
    }
}

/// The confinement an app asks for in its own manifest.
///
/// This exists because the sandbox settings are per-app and start *off*: an app
/// that knows it only ever talks to one API had no way to say so, and the user
/// had no way to know it was safe to narrow. Declaring it here means the
/// install applies it, and `force` means the dialog will not let it be turned
/// back off — for an app whose whole point is that it is confined.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SandboxDecl {
    /// Nothing was declared — leave whatever the user has set alone.
    pub declared: bool,
    /// Run confined. Applied on install; with `force` it cannot be turned off.
    pub enabled: bool,
    /// The user may not disable the sandbox or widen it beyond this
    /// declaration.
    pub force: bool,
    pub read_mode: Option<String>,
    pub network: Option<String>,
    pub hosts: Vec<String>,
    pub daemon_api: Option<bool>,
    pub loopback: Vec<u16>,
    /// `(path, read_only)` — extra folders the app needs.
    pub folders: Vec<(String, bool)>,
}

impl SandboxDecl {
    pub fn parse(manifest: &Value) -> SandboxDecl {
        let Some(sb) = manifest.get("sandbox") else {
            return SandboxDecl::default();
        };
        if !sb.is_object() {
            return SandboxDecl::default();
        }
        let force = sb
            .get("force")
            .or_else(|| sb.get("required"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        SandboxDecl {
            declared: true,
            // A forced declaration is by definition asking for the sandbox, so
            // `force: true` alone is enough — no one has to write both.
            enabled: sb.get("enabled").and_then(Value::as_bool).unwrap_or(force),
            force,
            read_mode: sb
                .get("readMode")
                .or_else(|| sb.get("read_mode"))
                .and_then(Value::as_str)
                .map(str::to_string),
            network: sb.get("network").and_then(Value::as_str).map(str::to_string),
            hosts: sb
                .get("hosts")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            daemon_api: sb
                .get("daemonApi")
                .or_else(|| sb.get("daemon_api"))
                .and_then(Value::as_bool),
            loopback: sb
                .get("loopback")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_u64)
                        .filter(|p| *p > 0 && *p < 65536)
                        .map(|p| p as u16)
                        .collect()
                })
                .unwrap_or_default(),
            folders: sb
                .get("folders")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|f| match f {
                            Value::String(s) => Some((s.clone(), false)),
                            Value::Object(o) => o
                                .get("path")
                                .and_then(Value::as_str)
                                .map(|p| {
                                    (
                                        p.to_string(),
                                        o.get("readOnly")
                                            .or_else(|| o.get("read_only"))
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false),
                                    )
                                }),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_app_that_says_nothing_is_a_session_app() {
        // The whole point of the change: the old always-on behaviour is now
        // opt-in, so a manifest written before this existed must come out as
        // `session` — not as `background` by inertia.
        let m = json!({"runtime": {"kind": "server", "start": "./crm", "port": 4390}});
        let rt = RuntimeSpec::parse(&m);
        assert_eq!(rt.mode, RunMode::Session);
        assert_eq!(rt.idle_timeout_secs, DEFAULT_IDLE_TIMEOUT_SECS);
        assert!(rt.is_server);
        assert_eq!(rt.runner, Runner::Binary);
    }

    #[test]
    fn background_is_spelled_the_ways_people_spell_it() {
        for s in ["background", "Background", "always", "daemon", "resident"] {
            assert_eq!(RunMode::parse(s), Some(RunMode::Background), "{s}");
        }
        for s in ["session", "on-demand", "lazy"] {
            assert_eq!(RunMode::parse(s), Some(RunMode::Session), "{s}");
        }
        // An unknown mode must not silently become background — an app the
        // daemon keeps alive forever by typo is the failure that costs money.
        assert_eq!(RunMode::parse("sometimes"), None);
        let m = json!({"runtime": {"kind": "server", "start": "./x", "mode": "sometimes"}});
        assert_eq!(RuntimeSpec::parse(&m).mode, RunMode::Session);
    }

    #[test]
    fn an_idle_timeout_is_never_short_enough_to_thrash() {
        let m = json!({"runtime": {"kind": "server", "start": "./x", "idleTimeoutSecs": 1}});
        assert_eq!(RuntimeSpec::parse(&m).idle_timeout_secs, MIN_IDLE_TIMEOUT_SECS);
        let m = json!({"runtime": {"kind": "server", "start": "./x", "idleTimeoutSecs": 600}});
        assert_eq!(RuntimeSpec::parse(&m).idle_timeout_secs, 600);
    }

    #[test]
    fn the_runner_is_inferred_from_the_start_command() {
        assert_eq!(Runner::infer("./crm"), Runner::Binary);
        assert_eq!(Runner::infer("npm start"), Runner::Node);
        assert_eq!(Runner::infer("node server.js"), Runner::Node);
        assert_eq!(Runner::infer("server.mjs"), Runner::Node);
        assert_eq!(Runner::infer("python3 -m app"), Runner::Python);
        assert_eq!(Runner::infer("main.py"), Runner::Python);
        assert_eq!(Runner::infer("uv run app.py"), Runner::Python);
        assert_eq!(Runner::infer("caddy run"), Runner::Shell);
        // An explicit declaration always wins over the guess.
        let m = json!({"runtime": {"kind": "server", "start": "./boot", "runner": "python"}});
        assert_eq!(RuntimeSpec::parse(&m).runner, Runner::Python);
    }

    #[test]
    fn a_python_app_gets_its_own_venv_unless_it_opts_out() {
        let m = json!({"runtime": {"kind": "server", "start": "python3 app.py"}});
        assert!(RuntimeSpec::parse(&m).venv, "never install into the system python");
        let m = json!({"runtime": {"kind": "server", "start": "python3 app.py", "venv": false}});
        assert!(!RuntimeSpec::parse(&m).venv);
        let m = json!({"runtime": {"kind": "server", "start": "npm start"}});
        assert!(!RuntimeSpec::parse(&m).venv, "a venv is a python thing");
    }

    #[test]
    fn requirements_read_both_spellings() {
        let m = json!({
            "requires": {
                "node": ">=18",
                "python": {"version": ">=3.10", "optional": true},
                "bin": ["ffmpeg", {"name": "git"}],
                "optionalBin": ["yt-dlp"],
                "env": ["OPENAI_API_KEY"],
                "os": ["macOS", "linux"]
            }
        });
        let r = Requires::parse(&m);
        assert_eq!(r.os, vec!["macos", "linux"]);
        let by = |n: &str| r.items.iter().find(|i| i.name == n).cloned().unwrap();
        assert_eq!(by("node").range.as_deref(), Some(">=18"));
        assert!(!by("node").optional);
        assert!(by("python").optional);
        assert_eq!(by("ffmpeg").kind, RequirementKind::Bin);
        assert!(!by("ffmpeg").optional);
        assert!(by("yt-dlp").optional);
        assert_eq!(by("OPENAI_API_KEY").kind, RequirementKind::Env);
        assert!(Requires::parse(&json!({})).is_empty());
    }

    #[test]
    fn a_forced_sandbox_declaration_is_on_without_saying_enabled_twice() {
        let m = json!({"sandbox": {"force": true, "network": "hosts", "hosts": ["api.openai.com"]}});
        let d = SandboxDecl::parse(&m);
        assert!(d.declared && d.force && d.enabled);
        assert_eq!(d.network.as_deref(), Some("hosts"));
        assert_eq!(d.hosts, vec!["api.openai.com"]);
        // Declared but not forced: applied on install, user may still change it.
        let d = SandboxDecl::parse(&json!({"sandbox": {"enabled": true, "readMode": "strict"}}));
        assert!(d.declared && d.enabled && !d.force);
        // Nothing declared: the user's own setting is not to be touched.
        assert!(!SandboxDecl::parse(&json!({})).declared);
    }

    #[test]
    fn sandbox_folders_accept_a_bare_path_or_a_read_only_object() {
        let m = json!({"sandbox": {"folders": ["/tmp/a", {"path": "/tmp/b", "readOnly": true}]}});
        let d = SandboxDecl::parse(&m);
        assert_eq!(d.folders, vec![("/tmp/a".into(), false), ("/tmp/b".into(), true)]);
    }
}

#[cfg(test)]
mod llm_decl_tests {
    use super::{LlmDecl, APP_DECLARABLE_ADAPTERS};
    use serde_json::json;

    #[test]
    fn an_app_without_an_llm_block_is_not_an_llm_app() {
        assert_eq!(LlmDecl::parse(&json!({})), Ok(None));
        assert_eq!(LlmDecl::parse(&json!({ "runtime": { "kind": "server" } })), Ok(None));
    }

    #[test]
    fn an_empty_block_takes_the_openai_defaults() {
        let d = LlmDecl::parse(&json!({ "llm": {} })).unwrap().unwrap();
        assert_eq!(d, LlmDecl::default());
        assert_eq!(d.path, "/v1");
        assert_eq!(d.adapt, "openai");
        assert!(!d.auto_register, "registration is opt-in, like mcp.autoRegister");
    }

    /// The whole reason `parse` returns a `Result`. Each of these would
    /// otherwise register a provider that fails at turn time with an error
    /// naming neither the app nor the field.
    #[test]
    fn a_misspelled_field_is_refused_rather_than_defaulted() {
        for bad in [
            json!({ "llm": { "adapt": "opanai" } }),
            json!({ "llm": { "adapt": "gemini" } }),
            json!({ "llm": { "adapt": 1 } }),
            json!({ "llm": { "path": "v1" } }),        // no leading slash
            json!({ "llm": { "path": "" } }),
            json!({ "llm": { "path": "/" } }),         // empties after trim
            json!({ "llm": { "autoRegister": "true" } }),
            json!({ "llm": { "displayName": 42 } }),
            json!({ "llm": [] }),
        ] {
            assert!(LlmDecl::parse(&bad).is_err(), "should be refused: {bad}");
        }
    }

    /// `local-mlx` routes the turn to an in-process engine, so an app declaring
    /// it would be registered and then never receive a single request.
    #[test]
    fn an_in_process_adapter_cannot_be_claimed_by_an_app() {
        for a in ["local-mlx", "local-candle-native", "codex", "antigravity"] {
            let m = json!({ "llm": { "adapt": a } });
            let err = LlmDecl::parse(&m).unwrap_err();
            assert!(err.contains(a), "the error must name the offending value: {err}");
        }
    }

    #[test]
    fn every_declarable_adapter_actually_parses() {
        for a in APP_DECLARABLE_ADAPTERS {
            let m = json!({ "llm": { "adapt": a } });
            assert_eq!(LlmDecl::parse(&m).unwrap().unwrap().adapt, *a);
        }
    }

    #[test]
    fn adapt_is_case_insensitive_but_path_is_not() {
        let d = LlmDecl::parse(&json!({ "llm": { "adapt": "OpenAI" } })).unwrap().unwrap();
        assert_eq!(d.adapt, "openai");
        // A trailing slash is trimmed so `path + "/chat/completions"` never
        // produces a double slash, which some routers 404 on.
        let d = LlmDecl::parse(&json!({ "llm": { "path": "/api/v1/" } })).unwrap().unwrap();
        assert_eq!(d.path, "/api/v1");
    }

    #[test]
    fn a_blank_display_name_is_none_not_an_empty_label() {
        let d = LlmDecl::parse(&json!({ "llm": { "displayName": "   " } })).unwrap().unwrap();
        assert_eq!(d.display_name, None);
        let d = LlmDecl::parse(&json!({ "llm": { "displayName": " MLX " } })).unwrap().unwrap();
        assert_eq!(d.display_name.as_deref(), Some("MLX"));
    }
}
