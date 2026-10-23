//! Did the agent actually finish? Evidence-based checks for when a coding
//! session stops.
//!
//! A small model's commonest failure is "done" on work it never ran, or on a
//! build that is still red. OpenHands answers with a critic (is the patch
//! empty?), a goal judge (an LLM scores the transcript and lists what is
//! missing) and a Stop hook that can send the agent back. These are the pieces
//! the loop in [`super::conversation`] puts together:
//!
//! - [`result_failed`]: did this tool result say the call did not succeed?
//!   (the signal behind "your last check failed and nothing passed since")
//! - [`Evidence`]: a short, bounded record of what the run did, for the judge.
//! - [`judge_prompt`] / [`parse_verdict`] / [`judge_feedback`]: the optional
//!   LLM judge. It is advisory and fails open: a reply that is not readable
//!   JSON never stops an agent from finishing.

/// Text a failed call starts with. Edit and friends report a refusal as a
/// normal result beginning with `Error:`.
const ERROR_PREFIXES: &[&str] = &["Error:", "Command execution failed", "Command timed out", "Sandboxed execution failed", "Internal task error"];

/// Whether a tool result says the call did not succeed.
///
/// `Bash` ends a failed command with `[exit code N]` (see `tools::bash`); a
/// run that exits 0 never carries the marker.
pub fn result_failed(tool: &str, content: &str, is_error: bool) -> bool {
    if is_error {
        return true;
    }
    let head = content.trim_start();
    if ERROR_PREFIXES.iter().any(|p| head.starts_with(p)) {
        return true;
    }
    tool == "Bash" && content.trim_end().rsplit_once("[exit code ").map_or(false, |(_, rest)| rest.trim_end_matches(']').trim().parse::<i32>().map_or(false, |c| c != 0))
}

/// What the run did, one short line per call, newest last.
#[derive(Debug, Default)]
pub struct Evidence {
    lines: Vec<String>,
}

const MAX_LINES: usize = 40;

impl Evidence {
    pub fn add(&mut self, label: &str, failed: bool) {
        let label: String = label.chars().take(120).collect();
        self.lines.push(format!("{label} -> {}", if failed { "FAILED" } else { "ok" }));
        if self.lines.len() > MAX_LINES {
            self.lines.remove(0);
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// System prompt of the judge call.
pub const JUDGE_SYSTEM: &str = "You check whether an AI coding assistant finished the user's request. Reply with one JSON object and nothing else: \
{\"complete\": true|false, \"missing\": [\"short sentence\", ...]}. \
Treat anything that is merely claimed, not shown by the evidence (a fix with no passing build or test run after it, a feature with no sign it was written), as NOT complete. \
List at most 3 concrete things that are missing; an empty list when complete.";

/// The user message of the judge call.
pub fn judge_prompt(request: &str, evidence: &[String], answer: &str) -> String {
    let request: String = request.chars().take(1500).collect();
    let answer: String = answer.chars().take(1500).collect();
    let mut out = format!("User request:\n{request}\n\nWhat the assistant did (oldest first):\n");
    if evidence.is_empty() {
        out.push_str("(no tool calls)\n");
    }
    for l in evidence {
        out.push_str("- ");
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&format!("\nThe assistant's final answer:\n{answer}\n"));
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub complete: bool,
    pub missing: Vec<String>,
}

/// Read the first complete JSON object in the judge's reply. `None` when there
/// is none or it has no boolean `complete`.
pub fn parse_verdict(reply: &str) -> Option<Verdict> {
    let start = reply.find('{')?;
    let mut stream = serde_json::Deserializer::from_str(&reply[start..]).into_iter::<serde_json::Value>();
    let value = stream.next()?.ok()?;
    let complete = value.get("complete")?.as_bool()?;
    let missing = match value.get("missing") {
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).take(3).collect(),
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
        _ => Vec::new(),
    };
    Some(Verdict { complete, missing })
}

/// What to send the agent back with.
pub fn judge_feedback(missing: &[String]) -> String {
    let mut out = String::from("(Automatic check - not a message from the user; do not mention it in your answer.) A check of your work against the request found it is not finished yet:\n");
    for m in missing {
        out.push_str("- ");
        out.push_str(m);
        out.push('\n');
    }
    out.push_str("Do the missing part (inspect the real current state, do not rely on memory), verify it, then give your final answer. If something cannot be done here, say so plainly instead of claiming it is done.");
    out
}

/// Sent once when the last build/test/lint command failed and nothing passed since.
/// Worded like [`super::conversation`]'s verify nudge: it says what it is, and what
/// the final answer should contain, because a small model answers a user-role message
/// as if the user had said it.
pub fn failed_check_nudge(command: &str) -> String {
    format!(
        "(Automatic check - not a message from the user; do not mention it in your answer.) \
         Your last build/test/lint run (`{command}`) failed and nothing has passed since, so the work is not done. \
         Fix what it reports and run it again. If you cannot, say plainly in your final answer that it still fails, quote the failure, \
         and say what you changed (path:line)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_bash_is_told_by_its_exit_marker() {
        assert!(result_failed("Bash", "test foo ... FAILED\n[exit code 101]", false));
        assert!(result_failed("Bash", "error: nope\n[exit code 1]", false));
        assert!(!result_failed("Bash", "all good\n", false));
        assert!(!result_failed("Bash", "(no output; exit code 0)", false));
        // A command that merely *prints* the marker mid-output is not a failure.
        assert!(!result_failed("Bash", "see [exit code 3] in the docs\nmore text after it", false));
        assert!(result_failed("Bash", "Command timed out after 120000ms", false));
    }

    #[test]
    fn other_tools_fail_by_error_text_or_flag() {
        assert!(result_failed("Edit", "Error: old_string not found in a.rs.", false));
        assert!(result_failed("Read", "anything", true));
        assert!(!result_failed("Edit", "Edited a.rs (1 replacement)", false));
        assert!(!result_failed("Read", "an Error: mentioned inside a file", false), "only a result that starts with it");
    }

    #[test]
    fn evidence_is_short_and_bounded() {
        let mut e = Evidence::default();
        for i in 0..100 {
            e.add(&format!("Edit file{i}.rs"), i % 10 == 0);
        }
        assert_eq!(e.lines().len(), MAX_LINES);
        assert!(e.lines().last().unwrap().ends_with("ok"));
        assert!(e.lines().iter().any(|l| l.ends_with("FAILED")));
    }

    #[test]
    fn the_judge_prompt_carries_the_request_the_evidence_and_the_answer() {
        let p = judge_prompt("fix the parser", &["Bash: cargo test -> FAILED".into()], "Done, all tests pass.");
        assert!(p.contains("fix the parser") && p.contains("cargo test -> FAILED") && p.contains("Done, all tests pass."));
        assert!(judge_prompt("x", &[], "y").contains("(no tool calls)"));
    }

    #[test]
    fn a_verdict_is_read_from_the_first_json_object() {
        let v = parse_verdict("Sure!\n```json\n{\"complete\": false, \"missing\": [\"run the tests\", \"\", \"update the docs\"]}\n```").unwrap();
        assert!(!v.complete);
        assert_eq!(v.missing, vec!["run the tests".to_string(), "update the docs".to_string()]);
        assert_eq!(parse_verdict("{\"complete\": true}").unwrap(), Verdict { complete: true, missing: vec![] });
        assert_eq!(parse_verdict("{\"complete\": false, \"missing\": \"tests\"}").unwrap().missing, vec!["tests".to_string()]);
    }

    #[test]
    fn an_unreadable_verdict_lets_the_agent_finish() {
        assert_eq!(parse_verdict("looks fine to me"), None);
        assert_eq!(parse_verdict("{\"score\": 0.9}"), None);
        assert_eq!(parse_verdict("{\"complete\": \"yes\"}"), None);
        assert_eq!(parse_verdict("{\"complete\": tru"), None);
    }

    #[test]
    fn the_feedback_lists_what_is_missing() {
        let f = judge_feedback(&["run the tests".into()]);
        assert!(f.contains("- run the tests") && f.contains("not finished"));
        assert!(failed_check_nudge("cargo test").contains("`cargo test`"));
    }
}
