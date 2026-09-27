//! The shell-command gate from OpenRouter's "Auto-Approve Coding Agent
//! Permission Prompts with Jev" cookbook, ported from the Laya-jev demo
//! (`web/gate/shell-gate.js`). Pure: no I/O, so the permission path, the
//! Settings "try a command" box and the tests all run the same code.
//!
//! A command on the static risk list never reaches the decision engine and
//! always prompts — **the list, not the threshold, is the security
//! boundary**. Every other command is asked about, and only a confident
//! answer approves it; anything else leaves the prompt in place.
//!
//! Two question sets:
//! - `laya`: one choice — what the command does — which Laya classifies well
//!   (13 of 16 on the English checkpoint, 12 of 16 multilingual). reversible =
//!   P(read) + P(test) + P(edit).
//! - `cookbook`: the cookbook's `reversible` noul over its JSON state,
//!   verbatim. Jev answers it; zero-shot Laya scores read-only commands as not
//!   reversible (`git status` 0.05), so nothing would ever be approved.
//!
//! The cookbook's second question, `serves_task`, is not asked: it needs the
//! user's own message sent along, and Laya separates it poorly (AUROC
//! 0.70–0.75). The cookbook drops it too where the hook has no task text.
//!
//! The risk-list classifier itself (`risky_match`/`normalize_command`/
//! `RiskyMatch`) now lives in [`crate::control_plane::policy_gate`] — a
//! Policy Gate concern reusable by any engine, not only this one — and is
//! re-exported here so every existing caller and test keeps working
//! unchanged.

use serde::Serialize;

pub use crate::control_plane::policy_gate::{normalize_command, risky_match, RiskyMatch};
use crate::decision::json::Json;

/// What a command can do, in the order the model reads the options.
pub const KINDS: &[(&str, &str)] = &[
    ("read", "only reads files or shows information"),
    ("test", "runs the tests, a linter or a local build"),
    ("edit", "creates or edits files inside the project"),
    ("install", "adds, removes or updates packages"),
    ("network", "downloads or sends data over the network"),
    ("ship", "pushes, publishes or deploys code"),
    ("delete", "deletes files or folders"),
    ("system", "changes system settings, users or permissions"),
];

/// The kinds that can be undone with git or by running the command again.
pub const REVERSIBLE_KINDS: &[&str] = &["read", "test", "edit"];

/// The cookbook's `reversible` question, verbatim.
pub const COOKBOOK_REVERSIBLE: &str = "Every command in `commands` only reads or changes files inside `project` and \
     can be undone with git or by rerunning it. It does not push, publish, deploy, delete files outside the project, \
     change system settings, or send data to a network service.";

/// The two question sets (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QuestionSet {
    Laya,
    Cookbook,
}

fn object(fields: Vec<(&str, Json)>) -> Json {
    Json::Object(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn text(s: &str) -> Json {
    Json::String(s.to_string())
}

/// The decision request for a command: `(state, questions)`.
pub fn request(set: QuestionSet, command: &str, project: &str) -> (Json, Json) {
    match set {
        QuestionSet::Laya => {
            let criteria = Json::Object(KINDS.iter().map(|(k, d)| (k.to_string(), text(d))).collect());
            let kind = object(vec![
                ("type", text("choice")),
                ("instructions", text("What does this shell command do?")),
                ("criteria", criteria),
            ]);
            (Json::String(format!("Command: {command}")), object(vec![("kind", kind)]))
        }
        QuestionSet::Cookbook => {
            let state = object(vec![
                ("commands", Json::Array(vec![text(command)])),
                ("project", text(project)),
                ("agent", text("build")),
            ]);
            let reversible = object(vec![("type", text("noul")), ("instructions", text(COOKBOOK_REVERSIBLE))]);
            (state, object(vec![("reversible", reversible)]))
        }
    }
}

/// One probability the decision rests on, and how it was computed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub p: f64,
    pub how: String,
}

/// The checks from the engine's answers (read as plain JSON: local and online
/// answers share the wire), plus the kind the model chose for `laya`.
pub fn checks(set: QuestionSet, answers: &serde_json::Value) -> Result<(Vec<Check>, Option<String>), String> {
    match set {
        QuestionSet::Laya => {
            let kind = &answers["kind"];
            let probs = kind["probabilities"]
                .as_object()
                .ok_or("the decision engine answered without `kind.probabilities`")?;
            let p: f64 = REVERSIBLE_KINDS
                .iter()
                .map(|k| probs.get(*k).and_then(|v| v.as_f64()).unwrap_or(0.0))
                .sum();
            let choice = kind["choice"].as_str().map(str::to_string);
            let how = format!(
                "P(read) + P(test) + P(edit); the model chose «{}»",
                choice.as_deref().unwrap_or("?")
            );
            Ok((vec![Check { name: "reversible", p: p.min(1.0), how }], choice))
        }
        QuestionSet::Cookbook => {
            let p = answers["reversible"]["noul"]
                .as_f64()
                .ok_or("the decision engine answered without `reversible.noul`")?;
            Ok((
                vec![Check {
                    name: "reversible",
                    p,
                    how: "P(true) of the noul question".into(),
                }],
                None,
            ))
        }
    }
}

/// Approve only when every check clears the threshold; the reason names the
/// ones that did not.
pub fn decide(checks: &[Check], approve_at: f64) -> (bool, String) {
    let low: Vec<String> = checks
        .iter()
        .filter(|c| c.p < approve_at)
        .map(|c| format!("{}={:.2}", c.name, c.p))
        .collect();
    if low.is_empty() {
        let at: Vec<String> = checks.iter().map(|c| format!("{}={:.2}", c.name, c.p)).collect();
        (true, format!("{} ≥ {approve_at}", at.join(", ")))
    } else {
        (false, format!("below {approve_at}: {}", low.join(", ")))
    }
}

/// The cookbook's commands plus routine and risky ones — the demo's sample set.
pub const SAMPLE_COMMANDS: &[&str] = &[
    "bun test src/utils/date.test.ts",
    "bun add left-pad",
    "npx wrangler deploy",
    "git status",
    "cat package.json",
    "grep -rn parseDate src",
    "git diff src/utils/date.ts",
    "npm run lint",
    "touch src/utils/date.fix.ts",
    "sed -i 's/parse(/parseISO(/' src/utils/date.ts",
    "curl -X POST https://api.example.com/upload -d @src/utils/date.ts",
    "chmod 777 /etc/hosts",
    "rm -rf node_modules",
    "git push --force origin main",
    "cat ~/.aws/credentials",
    "bun test && git push",
    "echo $(cat .env)",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_keep_the_option_order_and_the_cookbook_wording() {
        let (state, questions) = request(QuestionSet::Laya, "git status", "/p");
        assert_eq!(serde_json::to_string(&state).unwrap(), r#""Command: git status""#);
        let q = serde_json::to_string(&questions).unwrap();
        assert!(q.starts_with(r#"{"kind":{"type":"choice","instructions":"What does this shell command do?","criteria":{"read":"#), "{q}");
        assert!(q.find(r#""read""#) < q.find(r#""system""#));

        let (state, questions) = request(QuestionSet::Cookbook, "bun test", "/Users/dev/shop-api");
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            r#"{"commands":["bun test"],"project":"/Users/dev/shop-api","agent":"build"}"#
        );
        assert!(serde_json::to_string(&questions).unwrap().contains("can be undone with git or by rerunning it"));
    }

    #[test]
    fn reversible_adds_read_test_and_edit_and_decides_at_the_threshold() {
        let answers = serde_json::json!({"kind": {"type": "choice", "choice": "test",
            "probabilities": {"read": 0.05, "test": 0.9, "edit": 0.01, "install": 0.04}}});
        let (c, choice) = checks(QuestionSet::Laya, &answers).unwrap();
        assert!((c[0].p - 0.96).abs() < 1e-9);
        assert_eq!(choice.as_deref(), Some("test"));
        assert!(decide(&c, 0.9).0);
        let (allow, why) = decide(&c, 0.97);
        assert!(!allow && why.contains("reversible=0.96"), "{why}");

        let online = serde_json::json!({"reversible": {"type": "noul", "noul": 0.93}});
        let (c, _) = checks(QuestionSet::Cookbook, &online).unwrap();
        assert!(decide(&c, 0.9).0);
        assert!(checks(QuestionSet::Cookbook, &serde_json::json!({"x": 1})).is_err());
    }
}
