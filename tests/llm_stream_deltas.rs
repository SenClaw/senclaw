//! End-to-end check of the incremental-reply path.
//!
//! The voice chat only starts talking early if visible text reaches the client
//! *while the model is still writing*. That depends on `query_llm` calling its
//! delta sink per SSE event instead of once at the end — which is exactly what
//! was missing (nothing ever emitted `agent:delta`). These tests pin the two
//! properties the pipeline needs:
//!
//! 1. deltas are delivered **as they arrive**, before the request resolves;
//! 2. an SSE line split across TCP chunks (or mid-UTF-8) is still parsed, so
//!    Vietnamese text is not silently truncated.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use senclaw::zen_core::query_llm::{self, TextDeltaSink};
use senclaw::zen_core::{create_user_message, ContentBlock, ModelProfile};
use tokio_util::sync::CancellationToken;

fn profile(base_url: String, adapt: &str) -> ModelProfile {
    ModelProfile {
        name: "test".into(),
        provider: "test".into(),
        model_name: "test-model".into(),
        base_url,
        api_key: "k".into(),
        max_tokens: 1024,
        context_length: 8192,
        adapt: Some(adapt.into()),
        vision: None,
        oauth_provider: None,
        oauth_account_id: None,
    
        edit_format: None,
    }
}

/// Serve `chunks` as the response body, one TCP write each, with a small gap so
/// the client observes them separately — a stand-in for a model writing over
/// several seconds.
async fn serve(path: &'static str, chunks: Vec<Vec<u8>>) -> String {
    let body = move || {
        let chunks = chunks.clone();
        async move {
            let stream = futures::stream::unfold(chunks.into_iter(), |mut it| async move {
                let next = it.next()?;
                tokio::time::sleep(Duration::from_millis(20)).await;
                Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(next)), it))
            });
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
                .into_response()
        }
    };
    let app = Router::new().route(path, post(move || body()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Records each delta with the millisecond it landed, so the test can prove
/// they were not all flushed at the end.
#[derive(Default)]
struct Recorder {
    deltas: Mutex<Vec<(u128, String)>>,
}

fn sink(rec: &Arc<Recorder>, started: std::time::Instant) -> TextDeltaSink {
    let rec = Arc::clone(rec);
    Arc::new(move |d: &str| {
        rec.deltas
            .lock()
            .unwrap()
            .push((started.elapsed().as_millis(), d.to_string()));
    })
}

fn text_of(msg: &senclaw::zen_core::Message) -> String {
    msg.message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn openai_stream_emits_deltas_while_the_response_is_still_open() {
    let chunks: Vec<Vec<u8>> = vec![
        b"data: {\"choices\":[{\"delta\":{\"content\":\"Xin \"}}]}\n\n".to_vec(),
        b"data: {\"choices\":[{\"delta\":{\"content\":\"chao ban.\"}}]}\n\n".to_vec(),
        b"data: {\"choices\":[{\"delta\":{\"content\":\" Toi khoe.\"}}]}\n\n".to_vec(),
        b"data: [DONE]\n\n".to_vec(),
    ];
    let base = serve("/chat/completions", chunks).await;

    let rec = Arc::new(Recorder::default());
    let started = std::time::Instant::now();
    let s = sink(&rec, started);
    let msg = query_llm::query_llm(
        &reqwest::Client::new(),
        &[create_user_message(vec![ContentBlock::Text {
            text: "hi".into(),
        }])],
        "",
        &[],
        &CancellationToken::new(),
        &profile(base, "openai"),
        false,
        true,
        Some(&s),
    )
    .await
    .expect("stream request");
    let total_ms = started.elapsed().as_millis();

    let deltas = rec.deltas.lock().unwrap().clone();
    let pieces: Vec<String> = deltas.iter().map(|(_, d)| d.clone()).collect();
    assert_eq!(pieces, ["Xin ", "chao ban.", " Toi khoe."]);
    // The whole point: the first delta is in hand long before the turn ends.
    assert!(
        deltas[0].0 < total_ms,
        "first delta at {}ms, request finished at {total_ms}ms — nothing streamed early",
        deltas[0].0
    );
    assert_eq!(text_of(&msg), "Xin chao ban. Toi khoe.");
}

#[tokio::test]
async fn anthropic_stream_survives_lines_split_mid_utf8() {
    // "Chào bạn" split so that a chunk boundary lands inside the 2-byte `à`.
    let line = "data: {\"type\":\"content_block_delta\",\"delta\":\
                {\"type\":\"text_delta\",\"text\":\"Chào bạn\"}}\n\n";
    let bytes = line.as_bytes();
    let cut = line.find('à').unwrap() + 1;
    let chunks: Vec<Vec<u8>> = vec![
        bytes[..cut].to_vec(),
        bytes[cut..].to_vec(),
        b"data: {\"type\":\"message_stop\"}\n\n".to_vec(),
    ];
    let base = serve("/v1/messages", chunks).await;

    let rec = Arc::new(Recorder::default());
    let s = sink(&rec, std::time::Instant::now());
    let msg = query_llm::query_llm(
        &reqwest::Client::new(),
        &[create_user_message(vec![ContentBlock::Text {
            text: "hi".into(),
        }])],
        "",
        &[],
        &CancellationToken::new(),
        &profile(base, "anthropic"),
        false,
        true,
        Some(&s),
    )
    .await
    .expect("stream request");

    let pieces: Vec<String> = rec
        .deltas
        .lock()
        .unwrap()
        .iter()
        .map(|(_, d)| d.clone())
        .collect();
    assert_eq!(pieces, ["Chào bạn"], "split line must not be dropped");
    assert_eq!(text_of(&msg), "Chào bạn");
}
