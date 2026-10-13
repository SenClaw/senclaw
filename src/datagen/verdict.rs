//! The outcome of checking one candidate, and the retry rule that goes with it.
//!
//! Only `Error` and `Timeout` are retried. A `Failure` is a verdict about the
//! candidate itself ("wrong label", "too similar"): asking again would spend a
//! call to hear the same answer. An `Error` or `Timeout` says nothing about
//! the candidate — the model's reply was unreadable or never came — so one
//! more try is worth it.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The candidate passed.
    Success,
    /// The candidate was judged and rejected; the string is the reason code.
    Failure(String),
    /// The check could not reach a verdict (unparseable reply, provider error).
    Error(String),
    /// The check did not answer in time.
    Timeout,
}

impl Verdict {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Verdict::Error(_) | Verdict::Timeout)
    }

    /// Short reason code for counters and manifests.
    pub fn reason(&self) -> String {
        match self {
            Verdict::Success => "success".into(),
            Verdict::Failure(r) => r.clone(),
            Verdict::Error(_) => "error".into(),
            Verdict::Timeout => "timeout".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_errors_and_timeouts_are_retried() {
        assert!(Verdict::Error("unparseable".into()).is_retryable());
        assert!(Verdict::Timeout.is_retryable());
        assert!(!Verdict::Failure("label_mismatch".into()).is_retryable());
        assert!(!Verdict::Success.is_retryable());
        assert_eq!(Verdict::Failure("too_similar".into()).reason(), "too_similar");
    }
}
