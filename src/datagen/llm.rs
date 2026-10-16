//! The one model seam the pipelines call through.
//!
//! In the daemon it is [`DaemonLlm`]: the same one-shot completion the
//! patterns run uses (`llm_config::chat_completion`, active model or a named
//! profile), recorded in the usage table like every other call. Tests drive
//! the pipelines with a scripted fake instead.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use super::verdict::Verdict;

/// A per-call ceiling. Generous on purpose: a local model writing a batch of
/// requests at ~10 tok/s needs minutes, and a cut-off reply is worse than a
/// slow one. Cancelling the job interrupts a call at once regardless.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Default)]
pub struct LlmReply {
    pub text: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The provider stopped at the token cap (`finish_reason: "length"`): an
    /// unreadable reply is then a budget problem, not the model's format.
    pub truncated: bool,
}

#[async_trait]
pub trait Llm: Send + Sync {
    async fn complete(&self, system: &str, user: &str, max_tokens: u32) -> Result<LlmReply, String>;
}

/// The daemon's configured models.
pub struct DaemonLlm {
    pub config_path: PathBuf,
    /// LLM config id or label; `None` = the active model.
    pub profile: Option<String>,
    pub recorder: Option<Arc<crate::usage::UsageRecorder>>,
    /// What the usage table files the call under (`datagen:<dataset name>`).
    pub usage_label: String,
}

#[async_trait]
impl Llm for DaemonLlm {
    async fn complete(&self, system: &str, user: &str, max_tokens: u32) -> Result<LlmReply, String> {
        use crate::gateway::ui_server::llm_config::{chat_completion, record_completion};
        let r = chat_completion(&self.config_path, self.profile.as_deref(), system, user, max_tokens, None).await?;
        record_completion(&self.recorder, &self.usage_label, "", &r);
        let (input_tokens, output_tokens) = r
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens.or(u.input_tokens).unwrap_or(0), u.output()))
            .unwrap_or((0, 0));
        Ok(LlmReply { truncated: r.finish == "length", text: r.text, model: r.model, input_tokens, output_tokens })
    }
}

/// Why a call produced no reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The job was cancelled while the call was in flight.
    Cancelled,
    Failed(Verdict),
}

/// One call with the per-call ceiling and the job's cancellation.
pub async fn call(
    llm: &dyn Llm,
    cancel: &CancellationToken,
    system: &str,
    user: &str,
    max_tokens: u32,
) -> Result<LlmReply, CallError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(CallError::Cancelled),
        r = tokio::time::timeout(CALL_TIMEOUT, llm.complete(system, user, max_tokens)) => match r {
            Err(_) => Err(CallError::Failed(Verdict::Timeout)),
            Ok(Err(e)) => Err(CallError::Failed(Verdict::Error(e))),
            Ok(Ok(reply)) => Ok(reply),
        },
    }
}

/// Parse a JSON object out of a model reply that may wrap it in a code fence
/// or a sentence of preamble. `None` when no object in it deserializes.
pub fn parse_json<T: DeserializeOwned>(text: &str) -> Option<T> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str(t) {
        return Some(v);
    }
    let (start, end) = (t.find('{')?, t.rfind('}')?);
    if end <= start {
        return None;
    }
    serde_json::from_str(&t[start..=end]).ok()
}

#[cfg(test)]
pub mod fake {
    //! A scripted model for pipeline tests: answers come off a queue, every
    //! prompt is kept for assertions.

    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct ScriptedLlm {
        answers: Mutex<std::collections::VecDeque<Result<String, String>>>,
        pub prompts: Mutex<Vec<(String, String)>>,
    }

    impl ScriptedLlm {
        pub fn new(answers: Vec<Result<&str, &str>>) -> Self {
            ScriptedLlm {
                answers: Mutex::new(
                    answers.into_iter().map(|a| a.map(str::to_string).map_err(str::to_string)).collect(),
                ),
                prompts: Mutex::default(),
            }
        }
    }

    #[async_trait]
    impl Llm for ScriptedLlm {
        async fn complete(&self, system: &str, user: &str, _max: u32) -> Result<LlmReply, String> {
            self.prompts.lock().unwrap().push((system.to_string(), user.to_string()));
            let next = self.answers.lock().unwrap().pop_front().unwrap_or_else(|| Err("script exhausted".into()));
            next.map(|text| LlmReply { text, model: "scripted".into(), input_tokens: 3, output_tokens: 2, truncated: false })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Items {
        items: Vec<String>,
    }

    #[test]
    fn parses_bare_fenced_and_prefaced_json() {
        let want = Some(Items { items: vec!["a".into()] });
        assert_eq!(parse_json::<Items>(r#"{"items":["a"]}"#), want);
        assert_eq!(parse_json::<Items>("```json\n{\"items\":[\"a\"]}\n```"), want);
        assert_eq!(parse_json::<Items>("Here you go: {\"items\":[\"a\"]} hope it helps"), want);
        assert_eq!(parse_json::<Items>("no json here"), None);
        assert_eq!(parse_json::<Items>("{\"other\":1}"), None);
    }

    #[tokio::test]
    async fn a_cancelled_job_interrupts_the_call() {
        struct Never;
        #[async_trait]
        impl Llm for Never {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<LlmReply, String> {
                std::future::pending().await
            }
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(call(&Never, &cancel, "s", "u", 10).await.unwrap_err(), CallError::Cancelled);
    }
}
