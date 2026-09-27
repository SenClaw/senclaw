//! Local HTTP bridge for MCP send-server. Mirrors `src-old/agent/SendBridge.ts`.
//!
//! The MCP subprocess cannot access main-process channel instances directly.
//! It POSTs to `http://127.0.0.1:{port}/send`, and SendBridge routes via
//! injected callbacks to the target channel.  Listens on loopback only.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

pub type SendMessageFn = Arc<
    dyn Fn(String, String, Option<String>) -> futures::future::BoxFuture<'static, ()> + Send + Sync,
>;
pub type SendFileFn = Arc<
    dyn Fn(
            String,
            String,
            Option<String>,
            Option<String>,
        ) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum SendRequest {
    #[serde(rename = "message")]
    Message {
        chat_jid: String,
        text: String,
        bot_token: Option<String>,
    },
    #[serde(rename = "file")]
    File {
        chat_jid: String,
        file_path: String,
        caption: Option<String>,
        bot_token: Option<String>,
    },
}

#[derive(Debug, Serialize)]
struct SendResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub struct SendBridge {
    port: u16,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl SendBridge {
    pub async fn start(send_message: SendMessageFn, send_file: SendFileFn) -> Result<Self> {
        let app = Router::new().route(
            "/send",
            post(move |body: Json<SendRequest>| {
                let sm = Arc::clone(&send_message);
                let sf = Arc::clone(&send_file);
                async move {
                    match body.0 {
                        SendRequest::Message {
                            chat_jid,
                            text,
                            bot_token,
                        } => {
                            sm(chat_jid, text, bot_token).await;
                            Json(SendResponse {
                                ok: true,
                                error: None,
                            })
                        }
                        SendRequest::File {
                            chat_jid,
                            file_path,
                            caption,
                            bot_token,
                        } => {
                            sf(chat_jid, file_path, caption, bot_token).await;
                            Json(SendResponse {
                                ok: true,
                                error: None,
                            })
                        }
                    }
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind SendBridge")?;
        let port = listener.local_addr()?.port();
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .ok();
        });

        tracing::info!("[SendBridge] Listening on 127.0.0.1:{port}");
        Ok(Self {
            port,
            shutdown_tx: Some(tx),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for SendBridge {
    fn drop(&mut self) {
        self.stop();
    }
}
