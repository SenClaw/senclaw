//! Is the agent going in circles? Judged by what its calls *returned*, not by
//! what they were called.
//!
//! The loop used to treat a repeated identical call as a loop, always: the
//! second `cargo test` of a session was answered with "you already did this,
//! write your final answer", although an edit in between changed the answer.
//! The test–fix–retest cycle is what coding *is*. OpenHands' stuck detector
//! asks the right question - same action *and the same result*, again and
//! again - and also catches the loop the repeat counter cannot see, two
//! actions that undo each other (A, B, A, B, …).
//!
//! Three patterns over the last [`WINDOW`] calls:
//!
//! | pattern | meaning |
//! |---|---|
//! | [`Stuck::SameResult`] | the same call returned the same thing 4 times in a row |
//! | [`Stuck::Alternating`] | two calls took turns, with the same results, 3 rounds |
//! | [`Stuck::SameFailure`] | the same call failed with the same output 4 times, edits or not in between |
//!
//! The detector only reports. The loop nudges once, forgets the history so the
//! pattern has to build up again, and stops the run if it does.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

/// Calls remembered.
pub const WINDOW: usize = 30;
const SAME_RESULT: usize = 4;
const ALTERNATING_ROUNDS: usize = 3;
const SAME_FAILURE: usize = 4;

/// Longest prefix of a result that is compared: a log that differs only by a
/// timestamp 10,000 characters in is the same result for this purpose.
const COMPARED_BYTES: usize = 4096;

/// One executed call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// Tool name and arguments.
    pub sig: String,
    /// Fingerprint of what it returned.
    pub result: u64,
    /// It returned an error, a non-zero exit code or a timeout.
    pub failed: bool,
    /// What a person would call it: `Bash: cargo test`.
    pub label: String,
}

/// Fingerprint of a tool result.
pub fn fingerprint(text: &str) -> u64 {
    let mut cut = text.len().min(COMPARED_BYTES);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.len().min(1 << 20).hash(&mut h);
    text[..cut].hash(&mut h);
    h.finish()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stuck {
    SameResult { label: String, times: usize },
    Alternating { a: String, b: String },
    SameFailure { label: String, times: usize },
}

/// Opens every note the loop sends as a user-role message: a small model answers
/// those as if the user had said them, and quotes them in its final reply.
pub const AUTOMATIC: &str = "(Automatic check - not a message from the user; do not mention it in your answer.) ";

impl Stuck {
    /// One line for the nudge and the log.
    pub fn describe(&self) -> String {
        match self {
            Stuck::SameResult { label, times } => format!("`{label}` returned the same result {times} times in a row"),
            Stuck::Alternating { a, b } => format!("`{a}` and `{b}` keep taking turns with the same results"),
            Stuck::SameFailure { label, times } => format!("`{label}` failed with the same output {times} times"),
        }
    }

    /// What to tell the model when it first looks stuck.
    pub fn nudge(&self) -> String {
        let body = match self {
            Stuck::SameResult { label, times } => format!(
                "You have called `{label}` {times} times in a row and got exactly the same result each time. Calling it again changes nothing. \
                 Stop and change the approach: read the result again for what it actually says, look somewhere else, or tell the user plainly what blocks you."
            ),
            Stuck::Alternating { a, b } => format!(
                "You are alternating between `{a}` and `{b}` and the results keep repeating, so each step undoes the other. \
                 Step back: work out what each one is for, decide what the file or the state should finally be, and do that once - or tell the user what blocks you."
            ),
            Stuck::SameFailure { label, times } => format!(
                "`{label}` has failed with the same output {times} times although you changed things in between, so your changes are not reaching the cause. \
                 Read the failing output again, find the cause before you edit anything else, or tell the user plainly that it still fails and why."
            ),
        };
        format!("{AUTOMATIC}{body}")
    }
}

/// What the model is told when the nudge did not help and tools are taken away.
pub const FINAL_NUDGE: &str = "(Automatic check - not a message from the user; do not mention it in your answer.) You are stuck in a loop and tool access is now disabled. Do not claim the task is done. \
Tell the user, in their language, what you were trying to do, what keeps happening (quote the output), and what you would try next.";

#[derive(Debug, Default)]
pub struct Detector {
    calls: VecDeque<Call>,
}

impl Detector {
    pub fn record(&mut self, call: Call) {
        self.calls.push_back(call);
        if self.calls.len() > WINDOW {
            self.calls.pop_front();
        }
    }

    /// Forget everything: after a nudge the pattern must build up again.
    pub fn clear(&mut self) {
        self.calls.clear();
    }

    pub fn check(&self) -> Option<Stuck> {
        let n = self.calls.len();
        let last = self.calls.back()?;

        // The same call, the same result, in a row.
        let run = self.calls.iter().rev().take_while(|c| c.sig == last.sig && c.result == last.result).count();
        if run >= SAME_RESULT {
            return Some(Stuck::SameResult { label: last.label.clone(), times: run });
        }

        // A, B, A, B, A, B with the same results each time.
        let need = ALTERNATING_ROUNDS * 2;
        if n >= need {
            let tail: Vec<&Call> = self.calls.iter().rev().take(need).collect();
            let (x, y) = (tail[0], tail[1]);
            let differ = x.sig != y.sig;
            let repeats = tail.iter().enumerate().all(|(i, c)| {
                let want = if i % 2 == 0 { x } else { y };
                c.sig == want.sig && c.result == want.result
            });
            if differ && repeats {
                return Some(Stuck::Alternating { a: y.label.clone(), b: x.label.clone() });
            }
        }

        // The same failure, however much else happened between.
        if last.failed {
            let times = self.calls.iter().filter(|c| c.failed && c.sig == last.sig && c.result == last.result).count();
            if times >= SAME_FAILURE {
                return Some(Stuck::SameFailure { label: last.label.clone(), times });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(sig: &str, result: &str, failed: bool) -> Call {
        Call { sig: sig.into(), result: fingerprint(result), failed, label: sig.into() }
    }

    fn feed(d: &mut Detector, calls: &[(&str, &str, bool)]) {
        for (s, r, f) in calls {
            d.record(call(s, r, *f));
        }
    }

    #[test]
    fn the_same_call_with_a_new_result_is_progress_not_a_loop() {
        // test, fix, test, fix, test ...: identical command, but the output changes as the fix lands.
        let mut d = Detector::default();
        feed(&mut d, &[
            ("Bash: cargo test", "3 failed", true),
            ("Edit a.rs", "ok", false),
            ("Bash: cargo test", "2 failed", true),
            ("Edit a.rs", "ok", false),
            ("Bash: cargo test", "1 failed", true),
            ("Edit b.rs", "ok", false),
            ("Bash: cargo test", "ok", false),
        ]);
        assert_eq!(d.check(), None);
    }

    #[test]
    fn four_identical_results_in_a_row_are_a_loop() {
        let mut d = Detector::default();
        for _ in 0..3 {
            d.record(call("Read a.rs", "same text", false));
        }
        assert_eq!(d.check(), None, "three is still fine");
        d.record(call("Read a.rs", "same text", false));
        assert!(matches!(d.check(), Some(Stuck::SameResult { times: 4, .. })));
    }

    #[test]
    fn a_changed_result_breaks_the_run() {
        let mut d = Detector::default();
        feed(&mut d, &[("Bash: git status", "clean", false), ("Bash: git status", "clean", false), ("Bash: git status", "clean", false), ("Bash: git status", "1 file changed", false)]);
        assert_eq!(d.check(), None);
    }

    #[test]
    fn two_calls_that_undo_each_other_are_a_loop() {
        let mut d = Detector::default();
        for _ in 0..3 {
            feed(&mut d, &[("Edit a.rs: use tabs", "ok", false), ("Edit a.rs: use spaces", "ok", false)]);
        }
        match d.check() {
            Some(Stuck::Alternating { a, b }) => assert!(a != b),
            other => panic!("{other:?}"),
        }
        // Two rounds are not yet enough.
        let mut d = Detector::default();
        for _ in 0..2 {
            feed(&mut d, &[("A", "x", false), ("B", "y", false)]);
        }
        assert_eq!(d.check(), None);
    }

    #[test]
    fn the_same_failure_is_caught_even_with_edits_between() {
        let mut d = Detector::default();
        for i in 0..4 {
            feed(&mut d, &[("Bash: cargo test", "test foo ... FAILED", true)]);
            feed(&mut d, &[(&format!("Edit a.rs v{i}"), "ok", false)]);
            if i < 3 {
                assert_eq!(d.check(), None, "round {i}");
            }
        }
        feed(&mut d, &[("Bash: cargo test", "test foo ... FAILED", true)]);
        assert!(matches!(d.check(), Some(Stuck::SameFailure { times: 5, .. })), "{:?}", d.check());
    }

    #[test]
    fn a_successful_repeat_with_edits_between_is_not_a_failure_loop() {
        let mut d = Detector::default();
        for i in 0..6 {
            feed(&mut d, &[("Bash: cargo build", "Finished", false), (&format!("Edit x{i}"), "ok", false)]);
        }
        assert_eq!(d.check(), None);
    }

    #[test]
    fn after_a_nudge_the_history_is_forgotten() {
        let mut d = Detector::default();
        for _ in 0..4 {
            d.record(call("Read a", "same", false));
        }
        assert!(d.check().is_some());
        d.clear();
        assert_eq!(d.check(), None);
        d.record(call("Read a", "same", false));
        assert_eq!(d.check(), None, "the pattern has to build up again");
    }

    #[test]
    fn only_the_start_of_a_long_result_is_compared() {
        let a = format!("{}{}", "x".repeat(5000), "time 12:00");
        let b = format!("{}{}", "x".repeat(5000), "time 12:01");
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint("one"), fingerprint("two"));
        // A cut in the middle of a multi-byte character must not panic.
        let wide = "é".repeat(5000);
        let _ = fingerprint(&wide);
    }

    #[test]
    fn the_window_is_bounded() {
        let mut d = Detector::default();
        for i in 0..200 {
            d.record(call(&format!("c{i}"), "r", false));
        }
        assert_eq!(d.calls.len(), WINDOW);
    }

    #[test]
    fn the_nudge_names_the_call() {
        let s = Stuck::SameResult { label: "Bash: cargo test".into(), times: 4 };
        assert!(s.nudge().contains("`Bash: cargo test`") && s.describe().contains("4 times"));
        assert!(s.nudge().starts_with("(Automatic check"), "a small model must not read it as the user");
    }
}
