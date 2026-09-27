//! Serving an LLM **from** a Space App, so SenClaw can route turns to it.
//!
//! This is the reverse of [`crate::bridge`]. There, an app asks the daemon for
//! a completion. Here, the app *is* the model: it declares an `llm` block in its
//! `senclaw-manifest.json`, the daemon registers the models it advertises into
//! the same picker as every remote provider, and agent turns arrive over HTTP.
//!
//! ```ignore
//! use app_space_sdk::llm::{self, Chunk, ChatRequest, LlmProvider, ModelCard};
//!
//! struct Mlx { /* … */ }
//!
//! #[async_trait::async_trait]
//! impl LlmProvider for Mlx {
//!     fn models(&self) -> Vec<ModelCard> {
//!         vec![ModelCard::new("gemma-4-e2b-it-4bit", 128_000, 8192, true)]
//!     }
//!     async fn chat(&self, req: ChatRequest, sink: ChunkSink) -> anyhow::Result<()> {
//!         sink.send(Chunk::Text("hello".into())).await;
//!         Ok(())
//!     }
//! }
//!
//! let app = Router::new().merge(llm::openai_router(Arc::new(Mlx { })));
//! ```
//!
//! ## Why the app owns the wire format and not the provider
//!
//! The provider emits **semantic** events — visible text, reasoning, a tool call
//! — and this module renders them as OpenAI `chat.completion.chunk` SSE. That
//! split is the whole point: the daemon's OpenAI adapter is a real parser with
//! real expectations (`delta.content`, `delta.reasoning_content`, indexed
//! `delta.tool_calls` whose `name` and `arguments` *accumulate* across chunks),
//! and every app that hand-rolled that JSON would get a different corner of it
//! wrong. An app that implements [`LlmProvider`] cannot get it wrong at all.
//!
//! It also decides where parsing lives. A local model emits its tool calls as
//! *text* in whatever dialect its chat template uses; something has to turn that
//! into `tool_calls`. That something is the app, because the app is what holds
//! the model's own parser config. By the time bytes reach the daemon they are
//! ordinary OpenAI, and the daemon needs no special case for a local model —
//! which is what lets this reuse `adapt: "openai"` instead of adding an adapter.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Where the daemon looks for an app's model list while the app is **stopped**.
///
/// Relative to the app's own directory. A session app is stopped most of the
/// time — that is its resting state — and a model nobody can see in the picker
/// is a model nobody selects, calls, or ever starts the app for. So the list is
/// cached on disk at startup and read from there when the process is gone.
pub const MODELS_CACHE_PATH: &str = ".senclaw/llm-models.json";

// ============================================================================
// What a provider advertises
// ============================================================================

/// One model this app can serve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCard {
    /// Wire id. This is what arrives in `ChatRequest::model`, and what the user
    /// sees in the picker unless `display_name` says otherwise.
    pub id: String,
    /// Human label for the picker. Defaults to [`Self::id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Total context window, in tokens.
    pub context_length: u32,
    /// Ceiling on one response, in tokens.
    pub max_output_tokens: u32,
    /// **Required, never inferred.** SenClaw decides whether to send image
    /// blocks or fall back to OCR from this field, and the consequences are
    /// asymmetric: a text-only endpoint answers an image block with a hard 400
    /// that fails the entire turn, while OCR merely degrades it. Inference from
    /// the model id cannot be trusted here — a local checkpoint is named things
    /// like `mlx-community__Qwen3.5-2B-OptiQ-4bit`, which matches no vendor
    /// pattern, so a guess would land on `false` by accident today and `true`
    /// by accident the day someone widens a regex. The app has the model's
    /// `config.json` open; it knows.
    pub vision: bool,
    /// Whether the model can be given tools. `false` makes it a chat-only model
    /// in the picker.
    #[serde(default = "default_true")]
    pub tools: bool,
}

fn default_true() -> bool {
    true
}

impl ModelCard {
    pub fn new(id: impl Into<String>, context_length: u32, max_output_tokens: u32, vision: bool) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            context_length,
            max_output_tokens,
            vision,
            tools: true,
        }
    }

    pub fn display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    pub fn tools(mut self, tools: bool) -> Self {
        self.tools = tools;
        self
    }
}

// ============================================================================
// One turn
// ============================================================================

/// An incoming turn, in OpenAI `chat/completions` shape.
///
/// The modelled fields are the ones every provider needs. `raw` carries the
/// whole body besides, because SenClaw sends more than this struct names —
/// HF-style `tools`, `stream_options`, provider-specific extras — and a
/// provider that understands one of them should not have to fork the SDK to
/// read it.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Which [`ModelCard::id`] this turn is for.
    pub model: String,
    /// OpenAI-shaped messages, untouched. Passed through as JSON rather than a
    /// typed enum: `content` is a string on some turns and an array of parts on
    /// others (that is how images arrive), and a lossy re-encoding here would
    /// drop exactly the parts a vision model needs.
    pub messages: Vec<Value>,
    /// Tool definitions, or empty. OpenAI function shape.
    pub tools: Vec<Value>,
    /// Did the caller ask for SSE? [`openai_router`] handles both, so a provider
    /// normally ignores this — it is here for one that can genuinely go faster
    /// when nothing is watching.
    pub stream: bool,
    /// Output ceiling for this turn, when the caller set one.
    pub max_tokens: Option<u32>,
    /// Sampling temperature, when the caller set one.
    pub temperature: Option<f32>,
    /// The complete request body.
    pub raw: Value,
}

impl ChatRequest {
    fn from_body(body: Value) -> Result<Self, String> {
        let model = body["model"].as_str().unwrap_or("").to_string();
        if model.is_empty() {
            return Err("`model` is required".into());
        }
        let Some(messages) = body["messages"].as_array().cloned() else {
            return Err("`messages` must be an array".into());
        };
        if messages.is_empty() {
            return Err("`messages` must not be empty".into());
        }
        Ok(Self {
            model,
            messages,
            tools: body["tools"].as_array().cloned().unwrap_or_default(),
            stream: body["stream"].as_bool().unwrap_or(false),
            // `max_completion_tokens` is the current spelling; `max_tokens` is
            // what older clients (and SenClaw) still send.
            max_tokens: body["max_completion_tokens"]
                .as_u64()
                .or_else(|| body["max_tokens"].as_u64())
                .map(|v| v as u32),
            temperature: body["temperature"].as_f64().map(|v| v as f32),
            raw: body,
        })
    }
}

/// One semantic event from a running generation.
#[derive(Debug, Clone)]
pub enum Chunk {
    /// Visible assistant text, already stripped of any chat-template markers.
    Text(String),
    /// Chain-of-thought, shown separately by SenClaw and echoed back on the next
    /// request as `reasoning_content`.
    Reasoning(String),
    /// A completed tool call. Emit it whole: the SDK renders the accumulating
    /// `delta.tool_calls` shape the OpenAI wire requires, so a provider never
    /// has to stream partial JSON arguments and hope they reassemble.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// Token counts for this turn. Emit at most once, at the end. SenClaw reads
    /// it into its usage tracking; omitting it costs only the statistics.
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
    },
}

/// The handle a provider writes generation events to.
///
/// Sending after the client has disconnected is not an error — it is a no-op,
/// so a provider does not need to check. [`ChunkSink::is_closed`] is there for
/// one that would rather stop generating than finish into a void.
#[derive(Clone)]
pub struct ChunkSink(mpsc::Sender<Chunk>);

impl ChunkSink {
    pub async fn send(&self, chunk: Chunk) {
        let _ = self.0.send(chunk).await;
    }

    /// Convenience for the common case.
    pub async fn text(&self, s: impl Into<String>) {
        self.send(Chunk::Text(s.into())).await;
    }

    /// Has the receiving end gone away? A provider generating a long answer can
    /// poll this to abandon a turn whose client is no longer listening.
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

/// What an app implements to become a model.
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync + 'static {
    /// Every model this app can serve, right now.
    fn models(&self) -> Vec<ModelCard>;

    /// Run one turn, writing events to `sink` as they happen.
    ///
    /// Returning `Err` after events have already been sent ends the stream
    /// early; the client keeps what it received. Weights should be loaded here,
    /// lazily — **not** during startup. The daemon health-gates a newly spawned
    /// app on a 30-second budget with a 5-second probe timeout, so an app that
    /// loads gigabytes before it binds its port is reported as failing to start,
    /// with nothing in the error to say that loading was the reason.
    async fn chat(&self, req: ChatRequest, sink: ChunkSink) -> anyhow::Result<()>;
}

// ============================================================================
// The router
// ============================================================================

/// `/v1/models` + `/v1/chat/completions` for a provider.
///
/// Mount it wherever the manifest's `llm.path` says — `Router::merge` at the
/// root when that is `/v1`, or under `Router::nest` for anything else.
pub fn openai_router<P: LlmProvider>(provider: Arc<P>) -> Router {
    Router::new()
        .route("/v1/models", get(list_models::<P>))
        .route("/v1/chat/completions", post(chat_completions::<P>))
        .with_state(provider)
}

async fn list_models<P: LlmProvider>(State(p): State<Arc<P>>) -> Json<Value> {
    let data: Vec<Value> = p
        .models()
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id,
                "object": "model",
                "owned_by": "senclaw-space-app",
                // Not OpenAI fields. The daemon reads them to build the picker
                // entry; another OpenAI client ignores them.
                "display_name": m.display_name,
                "context_length": m.context_length,
                "max_output_tokens": m.max_output_tokens,
                "vision": m.vision,
                "tools": m.tools,
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data }))
}

async fn chat_completions<P: LlmProvider>(
    State(p): State<Arc<P>>,
    Json(body): Json<Value>,
) -> Response {
    let req = match ChatRequest::from_body(body) {
        Ok(r) => r,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, &msg),
    };
    if !p.models().iter().any(|m| m.id == req.model) {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("unknown model `{}`", req.model),
        );
    }

    let stream = req.stream;
    let model = req.model.clone();

    // Bounded, so a provider that outruns the client is slowed rather than
    // allowed to buffer a whole generation in memory.
    let (tx, mut rx) = mpsc::channel::<Chunk>(64);
    let sink = ChunkSink(tx);
    let generation = tokio::spawn(async move { p.chat(req, sink).await });

    if !stream {
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls: Vec<Value> = Vec::new();
        let mut usage: Option<Value> = None;
        while let Some(c) = rx.recv().await {
            accumulate(c, &mut text, &mut reasoning, &mut calls, &mut usage);
        }
        // The generation task owns the real error; a client that gets 200 with
        // half an answer and no explanation cannot tell a short reply from a
        // crash.
        match generation.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            Err(e) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("generation task panicked: {e}"),
                );
            }
        }
        return Json(non_stream_body(&model, &text, &reasoning, &calls, usage)).into_response();
    }

    let model_for_stream = model.clone();
    let sse = async_stream::stream! {
        let id = completion_id();
        let mut index = 0usize;
        while let Some(chunk) = rx.recv().await {
            if let Some(ev) = chunk_to_event(&id, &model_for_stream, chunk, &mut index) {
                yield Ok::<Event, std::convert::Infallible>(ev);
            }
        }

        // The status line went out with the first byte, so a failure here cannot
        // become a 5xx. Send it as an error chunk instead — silently ending the
        // stream would make a crashed generation look like a short answer, which
        // is the one reading a caller cannot recover from.
        let failure = match generation.await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(e) => Some(format!("generation task panicked: {e}")),
        };
        if let Some(msg) = failure {
            let body = json!({
                "id": id,
                "object": "chat.completion.chunk",
                "model": model_for_stream,
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "error" }],
                "error": { "message": msg, "type": "server_error" },
            });
            yield Ok(Event::default().data(body.to_string()));
        }

        // `[DONE]` closes the stream for the client's SSE reader. It is sent
        // even when the generation failed: the caller already has whatever text
        // arrived, and leaving the stream unterminated turns a failed turn into
        // a hung one that only the read timeout ends.
        yield Ok(Event::default().data("[DONE]"));
    };

    Sse::new(sse).into_response()
}

fn accumulate(
    chunk: Chunk,
    text: &mut String,
    reasoning: &mut String,
    calls: &mut Vec<Value>,
    usage: &mut Option<Value>,
) {
    match chunk {
        Chunk::Text(s) => text.push_str(&s),
        Chunk::Reasoning(s) => reasoning.push_str(&s),
        Chunk::ToolCall {
            id,
            name,
            arguments,
        } => calls.push(json!({
            "id": id,
            "type": "function",
            "function": { "name": name, "arguments": arguments },
        })),
        Chunk::Usage {
            prompt_tokens,
            completion_tokens,
        } => {
            *usage = Some(json!({
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            }));
        }
    }
}

/// Render one event as a `chat.completion.chunk`.
///
/// The tool-call shape is the fiddly part and the reason this is not left to
/// apps: the consumer accumulates `function.name` and `function.arguments` by
/// **concatenation** across chunks at a given `index`, so a whole call must go
/// out as a single delta at a fresh index. Sending the name twice, or reusing an
/// index, silently produces `get_weatherget_weather`.
fn chunk_to_event(id: &str, model: &str, chunk: Chunk, index: &mut usize) -> Option<Event> {
    let delta = match chunk {
        Chunk::Text(s) => {
            if s.is_empty() {
                return None;
            }
            json!({ "content": s })
        }
        Chunk::Reasoning(s) => {
            if s.is_empty() {
                return None;
            }
            json!({ "reasoning_content": s })
        }
        Chunk::ToolCall {
            id: call_id,
            name,
            arguments,
        } => {
            let i = *index;
            *index += 1;
            json!({
                "tool_calls": [{
                    "index": i,
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                }]
            })
        }
        Chunk::Usage {
            prompt_tokens,
            completion_tokens,
        } => {
            // Usage rides its own chunk with an empty `choices` array — the
            // shape `stream_options.include_usage` produces, and the one the
            // consumer looks for it in.
            let body = json!({
                "id": id,
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [],
                "usage": {
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": prompt_tokens + completion_tokens,
                },
            });
            return Some(Event::default().data(body.to_string()));
        }
    };

    let body = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": Value::Null }],
    });
    Some(Event::default().data(body.to_string()))
}

fn non_stream_body(
    model: &str,
    text: &str,
    reasoning: &str,
    calls: &[Value],
    usage: Option<Value>,
) -> Value {
    let mut message = json!({ "role": "assistant", "content": text });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    let mut out = json!({
        "id": completion_id(),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": if calls.is_empty() { "stop" } else { "tool_calls" },
        }],
    });
    if let Some(u) = usage {
        out["usage"] = u;
    }
    out
}

/// `chatcmpl-<hex>`. Uniqueness only has to hold within one client's stream, so
/// process id plus a monotonic counter is enough and pulls in no dependency.
fn completion_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "chatcmpl-{:x}{:x}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "message": message, "type": "invalid_request_error" } })),
    )
        .into_response()
}

// ============================================================================
// Model cache
// ============================================================================

/// Write the model list to [`MODELS_CACHE_PATH`], for the daemon to read while
/// this app is stopped. Call it once at startup, after the models are known.
///
/// An empty list is refused rather than written. The daemon treats a missing
/// cache as "not known yet" and a present one as authoritative, so clobbering a
/// good list with an empty one during a failed startup would remove the app's
/// models from the picker until someone noticed — the same rule the MCP tool
/// cache follows, for the same reason.
pub fn publish_models(app_dir: &std::path::Path, models: &[ModelCard]) -> anyhow::Result<()> {
    if models.is_empty() {
        anyhow::bail!("refusing to publish an empty model list");
    }
    let path = app_dir.join(MODELS_CACHE_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(&json!({ "models": models }))?;
    // Write-then-rename: a daemon reading this file concurrently sees either the
    // old list or the new one, never a truncated one.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> ModelCard {
        ModelCard::new("m", 4096, 512, false)
    }

    #[test]
    fn a_request_without_messages_is_refused_rather_than_run() {
        assert!(ChatRequest::from_body(json!({ "model": "m" })).is_err());
        assert!(ChatRequest::from_body(json!({ "model": "m", "messages": [] })).is_err());
        assert!(ChatRequest::from_body(json!({ "messages": [{ "role": "user" }] })).is_err());
    }

    #[test]
    fn both_spellings_of_the_output_ceiling_are_read() {
        let msgs = json!([{ "role": "user", "content": "hi" }]);
        let a = ChatRequest::from_body(json!({
            "model": "m", "messages": msgs, "max_tokens": 100
        }))
        .unwrap();
        assert_eq!(a.max_tokens, Some(100));

        // The newer spelling wins when a client sends both.
        let b = ChatRequest::from_body(json!({
            "model": "m", "messages": msgs, "max_tokens": 100, "max_completion_tokens": 200
        }))
        .unwrap();
        assert_eq!(b.max_tokens, Some(200));
    }

    /// Image turns arrive as an array of content parts. Re-encoding through a
    /// typed message would flatten that to a string and drop the image.
    #[test]
    fn multipart_content_survives_the_request_parse() {
        let req = ChatRequest::from_body(json!({
            "model": "m",
            "messages": [{ "role": "user", "content": [
                { "type": "text", "text": "what is this" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAA" } },
            ]}],
        }))
        .unwrap();
        let parts = req.messages[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAA");
    }

    #[test]
    fn empty_text_emits_no_chunk_at_all() {
        let mut index = 0;
        assert!(chunk_to_event("id", "m", Chunk::Text(String::new()), &mut index).is_none());
        assert!(chunk_to_event("id", "m", Chunk::Reasoning(String::new()), &mut index).is_none());
    }

    #[test]
    fn a_tool_call_turn_finishes_as_tool_calls_not_stop() {
        let calls = vec![json!({ "id": "c1" })];
        let body = non_stream_body("m", "", "", &calls, None);
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(non_stream_body("m", "hi", "", &[], None)["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn reasoning_is_omitted_when_empty_rather_than_sent_blank() {
        let body = non_stream_body("m", "hi", "", &[], None);
        assert!(body["choices"][0]["message"].get("reasoning_content").is_none());
        let body = non_stream_body("m", "hi", "why", &[], None);
        assert_eq!(body["choices"][0]["message"]["reasoning_content"], "why");
    }

    #[test]
    fn an_empty_model_list_never_clobbers_a_good_cache() {
        let dir = tempfile::tempdir().unwrap();
        publish_models(dir.path(), &[card()]).unwrap();
        let good = std::fs::read_to_string(dir.path().join(MODELS_CACHE_PATH)).unwrap();

        assert!(publish_models(dir.path(), &[]).is_err());
        let after = std::fs::read_to_string(dir.path().join(MODELS_CACHE_PATH)).unwrap();
        assert_eq!(good, after, "a failed publish must leave the cache intact");
    }

    // ── End-to-end over the real router ─────────────────────────────────────
    //
    // These drive an actual request through `openai_router` and read the bytes
    // that come back, because the bytes are the contract. SenClaw's OpenAI
    // adapter accumulates `delta.tool_calls[].function.{name,arguments}` by
    // *concatenation*, keyed on `index` — so a second call reusing index 0
    // produces `get_weatherget_time` with both argument objects glued together,
    // and no unit test of the event builder alone would notice.

    struct Fake(Vec<Chunk>);

    #[async_trait::async_trait]
    impl LlmProvider for Fake {
        fn models(&self) -> Vec<ModelCard> {
            vec![card()]
        }
        async fn chat(&self, _req: ChatRequest, sink: ChunkSink) -> anyhow::Result<()> {
            for c in &self.0 {
                sink.send(c.clone()).await;
            }
            Ok(())
        }
    }

    async fn post_chat(chunks: Vec<Chunk>, stream: bool) -> String {
        use tower::ServiceExt;
        let app = openai_router(Arc::new(Fake(chunks)));
        let body = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": stream,
        });
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Parse an SSE body back into the JSON objects a consumer would see.
    fn sse_payloads(raw: &str) -> Vec<Value> {
        raw.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
            .map(|d| serde_json::from_str(d).expect("every data: line must be JSON"))
            .collect()
    }

    #[tokio::test]
    async fn two_tool_calls_stream_at_distinct_indices() {
        let raw = post_chat(
            vec![
                Chunk::ToolCall {
                    id: "call_a".into(),
                    name: "get_weather".into(),
                    arguments: r#"{"city":"Hanoi"}"#.into(),
                },
                Chunk::ToolCall {
                    id: "call_b".into(),
                    name: "get_time".into(),
                    arguments: "{}".into(),
                },
            ],
            true,
        )
        .await;

        let calls: Vec<Value> = sse_payloads(&raw)
            .into_iter()
            .filter_map(|p| p["choices"][0]["delta"]["tool_calls"][0].as_object().cloned())
            .map(Value::Object)
            .collect();

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["index"], 0);
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[1]["index"], 1, "a reused index welds the two calls together");
        assert_eq!(calls[1]["function"]["name"], "get_time");
    }

    #[tokio::test]
    async fn a_stream_always_terminates_with_done() {
        let raw = post_chat(vec![Chunk::Text("hi".into())], true).await;
        assert!(
            raw.trim_end().ends_with("data: [DONE]"),
            "unterminated stream reads as a hang, not a failure:\n{raw}"
        );
    }

    /// Usage rides a chunk with an empty `choices` array — the shape produced by
    /// `stream_options.include_usage`, and the only place the consumer looks.
    #[tokio::test]
    async fn usage_arrives_on_its_own_chunk_with_no_choices() {
        let raw = post_chat(
            vec![
                Chunk::Text("hi".into()),
                Chunk::Usage {
                    prompt_tokens: 12,
                    completion_tokens: 3,
                },
            ],
            true,
        )
        .await;
        let usage = sse_payloads(&raw)
            .into_iter()
            .find(|p| p.get("usage").is_some())
            .expect("a usage chunk must be emitted");
        assert_eq!(usage["usage"]["prompt_tokens"], 12);
        assert_eq!(usage["usage"]["total_tokens"], 15);
        assert_eq!(usage["choices"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn non_stream_returns_one_assembled_message() {
        let raw = post_chat(
            vec![
                Chunk::Reasoning("thinking".into()),
                Chunk::Text("he".into()),
                Chunk::Text("llo".into()),
            ],
            false,
        )
        .await;
        let body: Value = serde_json::from_str(&raw).unwrap();
        let msg = &body["choices"][0]["message"];
        assert_eq!(msg["content"], "hello", "text deltas must be concatenated");
        assert_eq!(msg["reasoning_content"], "thinking");
        assert_eq!(body["object"], "chat.completion");
    }

    #[tokio::test]
    async fn an_unknown_model_is_404_not_a_silent_default() {
        use tower::ServiceExt;
        let app = openai_router(Arc::new(Fake(vec![])));
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({ "model": "nope", "messages": [{ "role": "user" }] }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    /// The daemon builds a picker entry from this, so `vision` must survive the
    /// hop — it decides between real image blocks and the OCR fallback.
    #[tokio::test]
    async fn models_endpoint_carries_the_capability_fields() {
        use tower::ServiceExt;
        let app = openai_router(Arc::new(Fake(vec![])));
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/models")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let m = &body["data"][0];
        assert_eq!(m["id"], "m");
        assert_eq!(m["context_length"], 4096);
        assert_eq!(m["vision"], false);
    }

    #[test]
    fn published_cards_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let m = ModelCard::new("gemma", 128_000, 8192, true).display_name("Gemma 4");
        publish_models(dir.path(), &[m]).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(MODELS_CACHE_PATH)).unwrap();
        let back: Value = serde_json::from_str(&raw).unwrap();
        let cards: Vec<ModelCard> = serde_json::from_value(back["models"].clone()).unwrap();
        assert_eq!(cards[0].id, "gemma");
        assert_eq!(cards[0].display_name.as_deref(), Some("Gemma 4"));
        assert!(cards[0].vision);
        assert!(cards[0].tools, "tools defaults to true when absent");
    }
}
