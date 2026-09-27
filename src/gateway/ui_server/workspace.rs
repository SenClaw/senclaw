//! Workspace file discovery — minimal read-only tree listing for the New Chat folder picker.

use std::path::Path;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use super::core::{AppError, UiState};
use crate::util::paths::expand_tilde;

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceQuery {
    /// Absolute or `~`-prefixed path to list. Tilde is expanded; relative input is rejected.
    pub path: String,
    /// Max depth of recursion (1 = direct children only). Defaults to 1.
    #[serde(default)]
    pub depth: Option<u32>,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkspaceEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkspaceListing {
    pub root: String,
    pub entries: Vec<WorkspaceEntry>,
}

fn is_skipped(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | ".git" | "target" | "dist" | "build" | ".next" | ".venv" | "__pycache__"
    ) || name.starts_with('.')
}

pub(crate) fn walk(root: &Path, depth_remaining: u32, out: &mut Vec<WorkspaceEntry>) {
    let rd = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut items: Vec<_> = rd.filter_map(Result::ok).collect();
    items.sort_by_key(|e| e.file_name());
    for entry in items {
        let name = entry.file_name().to_string_lossy().to_string();
        if is_skipped(&name) {
            continue;
        }
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = meta.is_dir();
        out.push(WorkspaceEntry {
            name,
            path: path.to_string_lossy().to_string(),
            is_dir,
            size: if is_dir { None } else { Some(meta.len()) },
        });
        if is_dir && depth_remaining > 1 {
            walk(&path, depth_remaining - 1, out);
        }
    }
}

/// GET /api/workspace/files?path=...&depth=1
pub(crate) async fn list_files(
    State(_s): State<Arc<UiState>>,
    Query(q): Query<WorkspaceQuery>,
) -> Result<Json<WorkspaceListing>, AppError> {
    let root = expand_tilde(&q.path);
    if !root.is_absolute() {
        return Err(AppError(
            axum::http::StatusCode::BAD_REQUEST,
            String::from("path must be absolute"),
        ));
    }
    if !root.exists() {
        return Err(AppError(
            axum::http::StatusCode::BAD_REQUEST,
            String::from("path does not exist"),
        ));
    }
    if !root.is_dir() {
        return Err(AppError(
            axum::http::StatusCode::BAD_REQUEST,
            String::from("path is not a directory"),
        ));
    }
    let depth = q.depth.unwrap_or(1).clamp(1, 4);
    let mut entries = Vec::new();
    walk(&root, depth, &mut entries);
    if entries.len() > 5000 {
        entries.truncate(5000);
    }
    Ok(Json(WorkspaceListing {
        root: root.to_string_lossy().to_string(),
        entries,
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct MentionQuery {
    /// Chat session to resolve the workspace from. Omit when `path` is given.
    pub jid: Option<String>,
    /// Explicit workspace root — used by the New Chat screen, which has no jid yet.
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MentionEntry {
    /// Path relative to `root` — this is what the user types after `@`.
    pub rel: String,
    pub is_dir: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct MentionListing {
    pub root: Option<String>,
    pub entries: Vec<MentionEntry>,
}

/// GET /api/chat/files?jid=…|path=… — candidates for the composer's `@` picker.
///
/// Returns paths *relative* to the workspace root, matching what
/// [`crate::agent::prompt_directives`] resolves on the way back in. A chat with
/// no workspace yields an empty list rather than an error: the composer just
/// shows no file suggestions.
pub(crate) async fn mention_files(
    State(s): State<Arc<UiState>>,
    Query(q): Query<MentionQuery>,
) -> Result<Json<MentionListing>, AppError> {
    let root = match resolve_mention_root(&s, &q) {
        Some(r) => r,
        None => {
            return Ok(Json(MentionListing {
                root: None,
                entries: vec![],
            }))
        }
    };

    let root_for_walk = root.clone();
    let mut raw = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        walk(&root_for_walk, 4, &mut out);
        out
    })
    .await
    .map_err(|e| {
        AppError(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("workspace scan failed: {e}"),
        )
    })?;

    raw.truncate(4000);
    let entries = raw
        .into_iter()
        .filter_map(|e| {
            let rel = Path::new(&e.path).strip_prefix(&root).ok()?;
            Some(MentionEntry {
                rel: rel.to_string_lossy().to_string(),
                is_dir: e.is_dir,
            })
        })
        .collect();

    Ok(Json(MentionListing {
        root: Some(root.to_string_lossy().to_string()),
        entries,
    }))
}

/// Mirrors `AgentPool::effective_work_dir` — an explicit `path` wins, else the
/// jid's bound workspace. Keep the two in step: a root the picker offers but
/// the expander rejects would surface as "file not found" on send.
fn resolve_mention_root(s: &UiState, q: &MentionQuery) -> Option<std::path::PathBuf> {
    if let Some(p) = q.path.as_deref().filter(|p| !p.trim().is_empty()) {
        let dir = expand_tilde(p);
        return dir.is_dir().then_some(dir);
    }
    let jid = q.jid.as_deref()?;
    let db = s.db.as_ref()?;
    let gm = s.group_manager.as_ref()?;
    let binding = gm.get(db, jid)?;
    let dir = expand_tilde(binding.allowed_work_dirs.as_ref()?.first()?);
    dir.is_dir().then_some(dir)
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkspaceFileContent {
    pub path: String,
    pub content: String,
    pub truncated: bool,
}

/// GET /api/workspace/file?path=... — read a single workspace file as UTF-8
/// text (capped at 512 KB; binary/over-size files return a notice instead).
pub(crate) async fn read_file(
    State(_s): State<Arc<UiState>>,
    Query(q): Query<WorkspaceQuery>,
) -> Result<Json<WorkspaceFileContent>, AppError> {
    use axum::http::StatusCode;
    let path = expand_tilde(&q.path);
    if !path.is_absolute() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "path must be absolute".into(),
        ));
    }
    if !path.is_file() {
        return Err(AppError(StatusCode::BAD_REQUEST, "not a file".into()));
    }
    const MAX: u64 = 512 * 1024;
    let meta = std::fs::metadata(&path)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let truncated = meta.len() > MAX;
    let bytes = std::fs::read(&path)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let slice = if truncated {
        &bytes[..MAX as usize]
    } else {
        &bytes[..]
    };
    let content = match std::str::from_utf8(slice) {
        Ok(s) => s.to_string(),
        Err(_) => "(binary file — cannot display)".to_string(),
    };
    Ok(Json(WorkspaceFileContent {
        path: path.to_string_lossy().to_string(),
        content,
        truncated,
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceMkdirRequest {
    /// Absolute or `~`-prefixed path of the folder to create.
    pub path: String,
    /// When true, also create any missing parent directories (mkdir -p).
    /// Defaults to true — the New Chat picker calls this for fresh project
    /// roots where the parent may also be new.
    #[serde(default = "default_recursive")]
    pub recursive: bool,
}

fn default_recursive() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkspaceMkdirResponse {
    pub path: String,
    /// True if this call actually created the directory; false if it was
    /// already present (idempotent — also success).
    pub created: bool,
}

/// POST /api/workspace/mkdir
/// Body: { path: "/abs/path", recursive?: true }
/// Creates a new workspace folder so the user can pick a fresh project root
/// directly from the New Chat picker without dropping to a shell.
pub(crate) async fn mkdir(
    State(_s): State<Arc<UiState>>,
    Json(body): Json<WorkspaceMkdirRequest>,
) -> Result<Json<WorkspaceMkdirResponse>, AppError> {
    let target = expand_tilde(body.path.trim());
    if !target.is_absolute() {
        return Err(AppError(
            axum::http::StatusCode::BAD_REQUEST,
            "path must be absolute".into(),
        ));
    }
    // Refuse paths under known system roots — a misclick shouldn't be able to
    // try `mkdir /` or `mkdir /System/...`. The picker is a developer tool but
    // the input box is freeform, so guard the obvious footguns.
    let s = target.to_string_lossy();
    for bad in [
        "/System", "/Library", "/usr", "/bin", "/sbin", "/etc", "/var", "/private",
    ] {
        if s == bad || s.starts_with(&format!("{bad}/")) {
            return Err(AppError(
                axum::http::StatusCode::FORBIDDEN,
                format!("refusing to create folder under system root {bad}"),
            ));
        }
    }
    if target.exists() {
        if target.is_dir() {
            return Ok(Json(WorkspaceMkdirResponse {
                path: target.to_string_lossy().to_string(),
                created: false,
            }));
        }
        return Err(AppError(
            axum::http::StatusCode::CONFLICT,
            "path exists and is not a directory".into(),
        ));
    }
    let res = if body.recursive {
        std::fs::create_dir_all(&target)
    } else {
        std::fs::create_dir(&target)
    };
    res.map_err(|e| {
        AppError(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("mkdir failed: {e}"),
        )
    })?;
    Ok(Json(WorkspaceMkdirResponse {
        path: target.to_string_lossy().to_string(),
        created: true,
    }))
}
