//! Pieces every generating pipeline shares: prompt fragments, budgets, and
//! how a run that produced nothing explains itself.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;

use super::generator::{AcceptLoop, Stop};
use super::jobs::JobCtx;
use super::llm::{call, parse_json, CallError, Llm};
use super::verdict::Verdict;

/// Default call budget is this many times the calls a perfect run needs.
pub const BUDGET_FACTOR: u32 = 3;
pub const DEFAULT_MAX_CALLS_CAP: u32 = 500;
pub const MAX_LLM_CALLS: u32 = 2000;

/// The call budget: the caller's, clamped, or `rounds × calls_per_round ×`
/// [`BUDGET_FACTOR`] within 10…[`DEFAULT_MAX_CALLS_CAP`].
pub fn budget(explicit: Option<u32>, rounds: u32, calls_per_round: u32) -> u32 {
    match explicit {
        Some(n) => n.clamp(1, MAX_LLM_CALLS),
        None => (rounds.max(1) * calls_per_round * BUDGET_FACTOR).clamp(10, DEFAULT_MAX_CALLS_CAP),
    }
}

pub fn language_rule(language: Option<&str>, fallback: &str) -> String {
    match language.map(str::trim).filter(|l| !l.is_empty()) {
        Some(l) => format!("Write in {l}."),
        None => fallback.to_string(),
    }
}

pub fn verdict_text(v: &Verdict) -> String {
    match v {
        Verdict::Error(e) => e.clone(),
        Verdict::Timeout => "the model did not answer in time".into(),
        other => other.reason(),
    }
}

pub fn reasons(rejected: &BTreeMap<String, u64>) -> String {
    if rejected.is_empty() {
        return "nothing was generated".into();
    }
    rejected.iter().map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(", ")
}

/// How a generating run ends: publish, note an early stop, and turn "nothing
/// accepted" into a failure that says why — the model's own error when every
/// rejection was a failed call or check, else the rejection counts.
pub fn finish(ctx: &JobCtx, l: &AcceptLoop, stop: Stop, last_error: Option<String>) -> Result<(), String> {
    ctx.publish(l);
    if let Some(note) = stop.note() {
        ctx.set_note(note);
    }
    if l.accepted == 0 {
        let only_call_failures = l
            .rejected
            .keys()
            .all(|k| k == "llm_error" || k.ends_with("_check_error") || k.starts_with("label_check_"));
        return Err(match last_error {
            Some(e) if stop == Stop::ProviderFailing || only_call_failures => e,
            _ => format!("no record passed ({})", reasons(&l.rejected)),
        });
    }
    Ok(())
}

/// A checking call that returns JSON of type `T`, retried once on an
/// unreadable reply; every attempt is counted against the budget. `Err(None)`
/// means the job was cancelled; `Err(Some(v))` is the verdict that ended it —
/// callers reject the batch on it (checks fail closed).
pub async fn checked_call<T: DeserializeOwned>(
    llm: &dyn Llm,
    ctx: &JobCtx,
    l: &mut AcceptLoop,
    system: &str,
    user: &str,
    max_tokens: u32,
    accept: impl Fn(&T) -> bool,
) -> Result<T, Option<Verdict>> {
    const TRIES: u32 = 2;
    let mut last = Verdict::Error("no attempt".into());
    for _ in 0..TRIES {
        if !l.can_spend(1) {
            return Err(Some(Verdict::Error("the LLM call budget was spent".into())));
        }
        let verdict = match call(llm, &ctx.cancel, system, user, max_tokens).await {
            Err(CallError::Cancelled) => return Err(None),
            Err(CallError::Failed(v)) => {
                l.record_call_error();
                v
            }
            Ok(reply) => {
                l.record_call(&reply);
                ctx.set_model(&reply.model);
                match parse_json::<T>(&reply.text) {
                    Some(v) if accept(&v) => return Ok(v),
                    Some(_) => Verdict::Error("the check returned an answer of the wrong shape".into()),
                    None => Verdict::Error("the check's reply was not JSON".into()),
                }
            }
        };
        if !verdict.is_retryable() {
            return Err(Some(verdict));
        }
        last = verdict;
    }
    Err(Some(last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_defaults_and_clamps() {
        assert_eq!(budget(None, 2, 2), 12);
        assert_eq!(budget(None, 1, 1), 10, "floor");
        assert_eq!(budget(None, 1000, 2), DEFAULT_MAX_CALLS_CAP);
        assert_eq!(budget(Some(0), 5, 5), 1);
        assert_eq!(budget(Some(99_999), 5, 5), MAX_LLM_CALLS);
    }

    #[test]
    fn reasons_read_as_counts() {
        let mut r = BTreeMap::new();
        r.insert("duplicate".to_string(), 3);
        r.insert("too_long".to_string(), 1);
        assert_eq!(reasons(&r), "3 duplicate, 1 too_long");
        assert_eq!(reasons(&BTreeMap::new()), "nothing was generated");
    }
}
