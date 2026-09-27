use axum::{
    body::Body,
    http::{header, StatusCode, Uri},
    response::Response,
};
use std::fs;
use std::path::PathBuf;

use super::types::path_to_mime;

/// SPA fallback: for any client-side route (e.g. `/chat/cowork:abc`) we
/// serve the right HTML shell so React-Router can take over on the
/// client. The Uri extractor reads the actual request path (the old
/// `x-original-uri` header trick required a reverse proxy), so direct
/// browser navigation works without nginx in the loop.
pub(crate) async fn spa_fallback(dist_dir: PathBuf, uri: Uri) -> Response {
    let path = uri.path();

    let is_wiki = path == "/wiki" || path.starts_with("/wiki/");
    let is_plugins = path == "/plugins" || path.starts_with("/plugins/");

    let fallback = if is_wiki {
        "wiki.html"
    } else if is_plugins {
        "plugins.html"
    } else {
        "index.html"
    };

    let file = dist_dir.join(fallback);
    match fs::read(&file) {
        Ok(contents) => {
            let mime = path_to_mime(fallback);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .body(Body::from(contents))
                .unwrap()
        }
        Err(_) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("Web UI not built. Run: npm run build:web"))
            .unwrap(),
    }
}
