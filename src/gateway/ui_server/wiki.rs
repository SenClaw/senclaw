use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use axum_extra::extract::Multipart;
use serde::Deserialize;

use crate::wiki::manager::WikiManager;

use super::core::{AppError, UiState};

// ===== Wiki helper =====

pub(crate) fn wiki_manager(s: &UiState) -> Result<&WikiManager, AppError> {
    s.wiki_manager.as_ref().map(|w| w.as_ref()).ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Wiki not initialized".into(),
        )
    })
}

// ===== Wiki API handlers =====

pub(crate) async fn wiki_tree(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let wm = wiki_manager(&s)?;
    let tree = wm
        .get_tree()
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "tree": tree })))
}

#[derive(Deserialize)]
pub(crate) struct WikiFileQuery {
    path: Option<String>,
}

pub(crate) async fn wiki_read(
    State(s): State<Arc<UiState>>,
    Query(q): Query<WikiFileQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let path = q
        .path
        .as_deref()
        .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "Missing path".into()))?;
    let wm = wiki_manager(&s)?;
    let doc = wm
        .read_file(path)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let json = serde_json::json!({
        "path": doc.path,
        "content": doc.content,
        "frontmatter": doc.frontmatter,
        "gitLog": doc.git_log,
    });
    Ok(Json(json))
}

#[derive(Deserialize)]
pub(crate) struct WikiWriteBody {
    path: String,
    content: String,
    #[serde(rename = "commitMsg")]
    commit_msg: Option<String>,
    source: Option<String>,
    tags: Option<Vec<String>>,
}

pub(crate) async fn wiki_write(
    State(s): State<Arc<UiState>>,
    Json(body): Json<WikiWriteBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if body.path.is_empty() || body.content.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Missing path or content".into(),
        ));
    }
    let wm = wiki_manager(&s)?;
    wm.write_file(
        &body.path,
        &body.content,
        body.source.as_deref(),
        body.tags.as_deref(),
        body.commit_msg.as_deref(),
    )
    .await
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "path": body.path,
        "updated": chrono::Utc::now().to_rfc3339(),
    })))
}

/// Turn a raw filename into a safe kebab-case wiki page slug (no extension).
fn slugify_filename(name: &str) -> String {
    // Drop any directory components and the extension.
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(name);
    let slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else if c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Collapse runs of '-' and trim edges.
    let collapsed: String = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        "document".to_string()
    } else {
        collapsed
    }
}

/// POST /api/wiki/upload — ingest one or more uploaded documents as wiki pages.
///
/// Multipart fields: `folder` (target dir, default `inbox`) and any number of
/// `file` fields. Each text-extractable file is converted to a `.md` page;
/// binary/unsupported files are reported as skipped rather than failing the
/// whole request.
pub(crate) async fn wiki_upload(
    State(s): State<Arc<UiState>>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let wm = wiki_manager(&s)?;

    let mut folder = String::from("inbox");
    let mut files: Vec<(String, String, Vec<u8>)> = Vec::new(); // (filename, content_type, bytes)

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, format!("Invalid multipart: {e}")))?
    {
        match field.name() {
            Some("folder") => {
                folder = field
                    .text()
                    .await
                    .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?
                    .trim()
                    .trim_matches('/')
                    .to_string();
            }
            Some("file") => {
                let filename = field.file_name().unwrap_or("document.txt").to_string();
                let content_type = field.content_type().unwrap_or("").to_string();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
                files.push((filename, content_type, bytes.to_vec()));
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    if files.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "No files uploaded".into(),
        ));
    }
    if folder.is_empty() {
        folder = "inbox".to_string();
    }
    if folder.contains("..") {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid folder".into()));
    }

    let mut created: Vec<String> = Vec::new();
    let mut skipped: Vec<serde_json::Value> = Vec::new();

    for (filename, content_type, bytes) in files {
        match crate::memory::cognitive::extract_text(&filename, &content_type, &bytes) {
            Ok(text) if !text.trim().is_empty() => {
                let slug = slugify_filename(&filename);
                let path = format!("{folder}/{slug}.md");
                let source = format!("upload:{filename}");
                match wm.write_file(&path, &text, Some(&source), None, None).await {
                    Ok(()) => created.push(path),
                    Err(e) => skipped.push(serde_json::json!({
                        "file": filename, "reason": e.to_string(),
                    })),
                }
            }
            Ok(_) => skipped.push(serde_json::json!({
                "file": filename, "reason": "no extractable text",
            })),
            Err(e) => skipped.push(serde_json::json!({
                "file": filename, "reason": e.to_string(),
            })),
        }
    }

    Ok(Json(serde_json::json!({
        "created": created,
        "skipped": skipped,
    })))
}

#[derive(Deserialize)]
pub(crate) struct WikiSearchQuery {
    q: Option<String>,
    tags: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn wiki_search(
    State(s): State<Arc<UiState>>,
    Query(q): Query<WikiSearchQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let query = q.q.unwrap_or_default();
    let tags: Option<Vec<String>> = q.tags.map(|t| {
        t.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });
    let limit = q.limit.unwrap_or(20);
    let wm = wiki_manager(&s)?;
    let results = wm
        .search(&query, tags.as_deref(), Some(limit))
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "results": results })))
}

pub(crate) async fn wiki_stats(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let wm = wiki_manager(&s)?;
    let stats = wm
        .get_stats()
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::to_value(stats).unwrap_or_default()))
}

#[derive(Deserialize)]
pub(crate) struct WikiHistoryQuery {
    path: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn wiki_history(
    State(s): State<Arc<UiState>>,
    Query(q): Query<WikiHistoryQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let path = q
        .path
        .as_deref()
        .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "Missing path".into()))?;
    let wm = wiki_manager(&s)?;
    let commits = wm
        .get_history(path, q.limit)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "commits": commits })))
}

pub(crate) async fn wiki_tags(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let wm = wiki_manager(&s)?;
    let tags = wm.get_tags();
    Ok(Json(serde_json::json!({ "tags": tags })))
}

#[derive(Deserialize)]
pub(crate) struct WikiMkdirBody {
    path: Option<String>,
}

pub(crate) async fn wiki_mkdir(
    State(s): State<Arc<UiState>>,
    Json(body): Json<WikiMkdirBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let path = body
        .path
        .as_deref()
        .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "Missing path".into()))?;
    let wm = wiki_manager(&s)?;
    wm.mkdir(path)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "path": path })))
}

#[derive(Deserialize)]
pub(crate) struct WikiDirDeleteQuery {
    path: Option<String>,
}

pub(crate) async fn wiki_dir_delete(
    State(s): State<Arc<UiState>>,
    Query(q): Query<WikiDirDeleteQuery>,
) -> Result<impl IntoResponse, AppError> {
    let path = q
        .path
        .as_deref()
        .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "Missing path".into()))?;
    let wm = wiki_manager(&s)?;
    wm.delete_empty_dir(path)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/wiki/file?path=... — remove a single wiki file (git-committed).
pub(crate) async fn wiki_file_delete(
    State(s): State<Arc<UiState>>,
    Query(q): Query<WikiFileQuery>,
) -> Result<impl IntoResponse, AppError> {
    let path = q
        .path
        .as_deref()
        .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "Missing path".into()))?;
    let wm = wiki_manager(&s)?;
    wm.delete_file(path)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}
