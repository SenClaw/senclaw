//! `/api/code/*` — code sessions as a thin view over ordinary chats.
//!
//! A *code session* is a chat whose binding has `group_type = "code"` and a
//! single working directory. Nothing here runs its own engine: a prompt sent
//! to a session goes through the same queue → AgentPool path as any chat, the
//! file tree is the chat's working directory, and "git log" / "rollback" are
//! the chat's shadow-git checkpoints ([`crate::checkpoints`]).
//!
//! The route names and JSON keys are the ones the mobile app
//! (`channel_app/lib/services/code_api.dart`) has called since the original
//! code engine was removed; they are kept so an installed app works without
//! an update. New clients should prefer the chat and checkpoint APIs directly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use super::core::{AppError, UiState};
use crate::types::GroupBinding;
use crate::util::paths::expand_tilde;

const SESSION_PREFIX: &str = "code:";
const ARCHIVED_KEY: &str = "code:archived:";
const LANG_KEY: &str = "code:lang:";

fn db(s: &Arc<UiState>) -> Result<Arc<crate::db::Db>, AppError> {
    s.db.clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "db_unset".into()))
}

fn bad(msg: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.into())
}

fn internal(e: impl std::fmt::Display) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn rfc3339_to_ms(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.timestamp_millis())
        .unwrap_or(0)
}

/// The working directory a code session is pinned to.
fn workspace_of(g: &GroupBinding) -> String {
    g.allowed_work_dirs
        .as_ref()
        .and_then(|d| d.first().cloned())
        .unwrap_or_default()
}

fn session_json(db: &crate::db::Db, g: &GroupBinding) -> serde_json::Value {
    let ws = workspace_of(g);
    let archived = matches!(db.get_router_state(&format!("{ARCHIVED_KEY}{}", g.jid)), Ok(Some(v)) if v == "1");
    let language = db
        .get_router_state(&format!("{LANG_KEY}{}", g.jid))
        .ok()
        .flatten();
    serde_json::json!({
        "id": g.jid,
        "name": g.name,
        "workspace": ws,
        "language": language,
        "status": if archived { "archived" } else { "active" },
        "git_enabled": Path::new(&ws).join(".git").exists(),
        "created_at": rfc3339_to_ms(&g.added_at),
        "updated_at": g.last_active.as_deref().map(rfc3339_to_ms).unwrap_or_else(|| rfc3339_to_ms(&g.added_at)),
    })
}

fn load_session(db: &crate::db::Db, id: &str) -> Result<GroupBinding, AppError> {
    db.get_group(id)
        .map_err(internal)?
        .filter(|g| g.group_type == "code")
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "code session not found".into()))
}

fn session_workspace(g: &GroupBinding) -> Result<PathBuf, AppError> {
    let ws = workspace_of(g);
    if ws.is_empty() {
        return Err(bad("session has no workspace"));
    }
    let dir = expand_tilde(&ws);
    if !dir.is_dir() {
        return Err(bad(format!("workspace {} does not exist", dir.display())));
    }
    Ok(dir)
}

/// Guess the dominant language from file extensions two levels deep.
fn guess_language(dir: &Path) -> Option<String> {
    let mut counts: std::collections::HashMap<&'static str, usize> = Default::default();
    let mut stack = vec![(dir.to_path_buf(), 0u8)];
    let mut seen = 0usize;
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 4000 {
                break;
            }
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if depth < 2 && !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            let lang = match p.extension().and_then(|x| x.to_str()) {
                Some("rs") => "rust",
                Some("ts") | Some("tsx") => "typescript",
                Some("js") | Some("jsx") | Some("mjs") => "javascript",
                Some("py") => "python",
                Some("go") => "go",
                Some("dart") => "dart",
                Some("java") | Some("kt") => "java",
                Some("swift") => "swift",
                Some("c") | Some("h") | Some("cc") | Some("cpp") | Some("hpp") => "c",
                Some("cs") => "csharp",
                Some("rb") => "ruby",
                Some("php") => "php",
                _ => continue,
            };
            *counts.entry(lang).or_default() += 1;
        }
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(l, _)| l.to_string())
}

const SKIP_DIRS: &[&str] = &[
    "node_modules", "target", ".git", ".venv", "venv", "__pycache__", ".dart_tool", "build", "dist", ".next", ".cache",
];

// ===== Sessions =====

#[derive(Deserialize, Default)]
pub(crate) struct ListQuery {
    #[serde(default)]
    status: Option<String>,
}

/// GET /api/code/sessions?status=active|archived|all
pub(crate) async fn sessions_list(
    State(s): State<Arc<UiState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let want = q.status.as_deref().unwrap_or("active");
    let mut out = Vec::new();
    for g in db.list_groups().map_err(internal)? {
        if g.group_type != "code" {
            continue;
        }
        let j = session_json(&db, &g);
        let status = j["status"].as_str().unwrap_or("active");
        if want == "all" || want == status {
            out.push(j);
        }
    }
    out.sort_by_key(|j| std::cmp::Reverse(j["updated_at"].as_i64().unwrap_or(0)));
    Ok(Json(serde_json::json!({ "sessions": out })))
}

#[derive(Deserialize)]
pub(crate) struct CreateBody {
    name: String,
    workspace: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    init_git: bool,
    /// Agent profile folder; defaults to `main`.
    #[serde(default)]
    folder: Option<String>,
}

/// POST /api/code/sessions
pub(crate) async fn sessions_create(
    State(s): State<Arc<UiState>>,
    Json(body): Json<CreateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let gm = s
        .group_manager
        .clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "group_manager_unset".into()))?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(bad("name is required"));
    }
    let dir = expand_tilde(body.workspace.trim());
    if !dir.is_absolute() {
        return Err(bad("workspace must be an absolute path"));
    }
    std::fs::create_dir_all(&dir).map_err(|e| bad(format!("cannot create workspace: {e}")))?;
    if body.init_git && !dir.join(".git").exists() {
        let st = tokio::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .await
            .map_err(|e| bad(format!("git init failed: {e}")))?;
        if !st.success() {
            return Err(bad("git init failed"));
        }
    }
    let now = chrono::Utc::now().to_rfc3339();
    let jid = format!("{SESSION_PREFIX}{}", uuid::Uuid::new_v4());
    let binding = GroupBinding {
        jid: jid.clone(),
        folder: body.folder.clone().filter(|f| !f.trim().is_empty()).unwrap_or_else(|| "main".into()),
        name: name.to_string(),
        channel: String::new(),
        group_type: "code".into(),
        requires_trigger: false,
        allowed_tools: None,
        allowed_paths: None,
        allowed_work_dirs: Some(vec![dir.to_string_lossy().to_string()]),
        bot_token: None,
        max_messages: None,
        llm_config_id: None,
        last_active: Some(now.clone()),
        added_at: now,
    };
    gm.register(&db, &s.config, &binding);
    let language = body
        .language
        .filter(|l| !l.trim().is_empty())
        .or_else(|| guess_language(&dir));
    if let Some(l) = &language {
        let _ = db.set_router_state(&format!("{LANG_KEY}{jid}"), l);
    }
    if let Some(api) = &s.agent_api {
        api.set_working_dir(&jid, &dir.to_string_lossy());
    }
    Ok(Json(session_json(&db, &binding)))
}

/// GET /api/code/sessions/:id
pub(crate) async fn sessions_get(
    State(s): State<Arc<UiState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let g = load_session(&db, &id)?;
    Ok(Json(session_json(&db, &g)))
}

/// DELETE /api/code/sessions/:id — archive (the chat and its history stay;
/// the session just leaves the active list).
pub(crate) async fn sessions_archive(
    State(s): State<Arc<UiState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    load_session(&db, &id)?;
    db.set_router_state(&format!("{ARCHIVED_KEY}{id}"), "1")
        .map_err(internal)?;
    Ok(Json(serde_json::json!({ "ok": true, "id": id, "status": "archived" })))
}

// ===== Files =====

const MAX_TREE_ENTRIES: usize = 5000;
const MAX_TREE_DEPTH: u8 = 8;

fn tree(dir: &Path, rel: &str, depth: u8, budget: &mut usize) -> Vec<serde_json::Value> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| {
        let is_dir = e.path().is_dir();
        (!is_dir, e.file_name().to_string_lossy().to_lowercase())
    });
    let mut out = Vec::new();
    for e in entries {
        if *budget == 0 {
            break;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let p = e.path();
        let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        *budget -= 1;
        if p.is_dir() {
            let children = if depth < MAX_TREE_DEPTH {
                tree(&p, &child_rel, depth + 1, budget)
            } else {
                Vec::new()
            };
            out.push(serde_json::json!({ "name": name, "path": child_rel, "type": "dir", "children": children }));
        } else {
            out.push(serde_json::json!({ "name": name, "path": child_rel, "type": "file" }));
        }
    }
    out
}

/// GET /api/code/sessions/:id/files
pub(crate) async fn sessions_files(
    State(s): State<Arc<UiState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let g = load_session(&db, &id)?;
    let dir = session_workspace(&g)?;
    let mut budget = MAX_TREE_ENTRIES;
    let t = tokio::task::spawn_blocking(move || {
        let t = tree(&dir, "", 0, &mut budget);
        (dir, t)
    })
    .await
    .map_err(internal)?;
    Ok(Json(serde_json::json!({ "workspace": t.0.to_string_lossy(), "tree": t.1 })))
}

#[derive(Deserialize)]
pub(crate) struct FileQuery {
    path: String,
}

/// Resolve a workspace-relative path and refuse anything that escapes.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, AppError> {
    let rel = rel.trim_start_matches("./");
    if rel.is_empty() {
        return Err(bad("path is required"));
    }
    if Path::new(rel).is_absolute() || rel.split(['/', '\\']).any(|c| c == "..") {
        return Err(bad("path must be relative to the workspace"));
    }
    Ok(root.join(rel))
}

const MAX_FILE_CONTENT: u64 = 2 * 1024 * 1024;

/// GET /api/code/sessions/:id/file-content?path=
pub(crate) async fn sessions_file_content(
    State(s): State<Arc<UiState>>,
    AxPath(id): AxPath<String>,
    Query(q): Query<FileQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let g = load_session(&db, &id)?;
    let dir = session_workspace(&g)?;
    let p = safe_join(&dir, &q.path)?;
    let meta = std::fs::metadata(&p).map_err(|_| AppError(StatusCode::NOT_FOUND, "file not found".into()))?;
    if !meta.is_file() {
        return Err(bad("not a file"));
    }
    if meta.len() > MAX_FILE_CONTENT {
        return Err(bad(format!("file is {} bytes; limit is {MAX_FILE_CONTENT}", meta.len())));
    }
    let bytes = std::fs::read(&p).map_err(internal)?;
    let content = String::from_utf8_lossy(&bytes).to_string();
    Ok(Json(serde_json::json!({ "path": q.path, "content": content, "size": meta.len() })))
}

// ===== Checkpoints as git log / rollback =====

fn checkpoints(s: &Arc<UiState>) -> Result<Arc<crate::checkpoints::CheckpointService>, AppError> {
    s.checkpoints
        .clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "checkpoints_unset".into()))
}

/// GET /api/code/sessions/:id/git-log — the chat's checkpoints, newest first.
pub(crate) async fn sessions_git_log(
    State(s): State<Arc<UiState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    load_session(&db, &id)?;
    let cps = checkpoints(&s)?.list(&id).map_err(internal)?;
    let log: Vec<serde_json::Value> = cps
        .iter()
        .map(|c| {
            serde_json::json!({
                "hash": c.sha,
                "message": if c.parent_sha.is_some() { format!("{}: {}", c.tool_name, c.summary) } else { "baseline".to_string() },
                "date": c.created_at,
                "checkpoint_id": c.id,
                "files_changed": c.files_changed,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "log": log })))
}

#[derive(Deserialize, Default)]
pub(crate) struct RollbackBody {
    /// How many checkpoints back from the newest (1 = the previous one).
    #[serde(default)]
    steps: Option<usize>,
    /// Or an explicit checkpoint id.
    #[serde(default)]
    checkpoint_id: Option<i64>,
}

/// POST /api/code/sessions/:id/rollback — restore the working tree to a
/// previous checkpoint. Recorded as a checkpoint itself, so it can be undone.
pub(crate) async fn sessions_rollback(
    State(s): State<Arc<UiState>>,
    AxPath(id): AxPath<String>,
    body: Option<Json<RollbackBody>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    load_session(&db, &id)?;
    let svc = checkpoints(&s)?;
    let body = body.map(|b| b.0).unwrap_or_default();
    let target = match body.checkpoint_id {
        Some(cid) => cid,
        None => {
            let steps = body.steps.unwrap_or(1).max(1);
            let list = svc.list(&id).map_err(internal)?;
            list.get(steps)
                .map(|c| c.id)
                .ok_or_else(|| bad(format!("only {} checkpoint(s) to go back to", list.len().saturating_sub(1))))?
        }
    };
    let (report, cp) = svc.restore(&id, target, &[]).await.map_err(|e| bad(e.to_string()))?;
    if let (Some(cp), Some(api)) = (&cp, &s.agent_api) {
        api.broadcast_checkpoint_new(&id, cp);
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "restored": report.restored.len(),
        "removed": report.removed.len(),
        "checkpoint": cp,
    })))
}

// ===== Chat groups / messages (one group per session) =====

fn group_json(g: &GroupBinding) -> serde_json::Value {
    serde_json::json!({
        "id": g.jid,
        "project_id": g.jid,
        "name": g.name,
        "created_at": rfc3339_to_ms(&g.added_at),
        "updated_at": g.last_active.as_deref().map(rfc3339_to_ms).unwrap_or_else(|| rfc3339_to_ms(&g.added_at)),
    })
}

/// GET /api/code/projects/:id/groups — a session is its own (only) group.
pub(crate) async fn projects_groups(
    State(s): State<Arc<UiState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let g = load_session(&db, &id)?;
    Ok(Json(serde_json::json!({ "groups": [group_json(&g)] })))
}

/// POST /api/code/projects/:id/groups — kept for the client that asks; a
/// session has exactly one conversation, so this answers with it.
pub(crate) async fn projects_groups_create(
    State(s): State<Arc<UiState>>,
    AxPath(id): AxPath<String>,
    _body: Option<Json<serde_json::Value>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let g = load_session(&db, &id)?;
    Ok(Json(group_json(&g)))
}

fn messages_json(db: &crate::db::Db, jid: &str, s: &Arc<UiState>) -> Result<Vec<serde_json::Value>, AppError> {
    let rows = db.get_group_messages_after_ms(jid, -1, 500).map_err(internal)?;
    let processing = s
        .agent_states
        .as_ref()
        .and_then(|m| m.try_lock().ok().map(|g| g.get(jid).cloned()))
        .flatten()
        .map(|st| st == "processing")
        .unwrap_or(false);
    let n = rows.len();
    Ok(rows
        .iter()
        .enumerate()
        .map(|(i, (m, ms))| {
            let last_user_pending = processing && i + 1 == n && !m.is_bot_reply;
            serde_json::json!({
                "id": m.message_id,
                "role": if m.is_bot_reply { "assistant" } else { "user" },
                "content": m.content,
                "status": if last_user_pending { "processing" } else { "done" },
                "created_at": ms,
                "processed_at": if m.is_bot_reply { Some(*ms) } else { None },
            })
        })
        .collect())
}

/// GET /api/code/groups/:gid/messages
pub(crate) async fn groups_messages(
    State(s): State<Arc<UiState>>,
    AxPath(gid): AxPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    load_session(&db, &gid)?;
    Ok(Json(serde_json::json!({ "messages": messages_json(&db, &gid, &s)? })))
}

#[derive(Deserialize)]
pub(crate) struct ChatBody {
    prompt: String,
    #[serde(default)]
    group_id: Option<String>,
}

/// POST /api/code/sessions/:id/chat — same path as a chat message: queue →
/// AgentPool. Answers immediately with the message snapshot; the reply
/// arrives over the WS / relay events the client already listens to.
pub(crate) async fn sessions_chat(
    State(s): State<Arc<UiState>>,
    AxPath(id): AxPath<String>,
    Json(body): Json<ChatBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let g = load_session(&db, &id)?;
    if let Some(gid) = &body.group_id {
        if gid != &id {
            return Err(bad("group_id does not belong to this session"));
        }
    }
    let prompt = body.prompt.trim();
    if prompt.is_empty() {
        return Err(bad("prompt is empty"));
    }
    let api = s
        .agent_api
        .clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "agent_api_unset".into()))?;
    api.submit_user_message(&g, prompt).map_err(|e| AppError(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "session_id": id,
        "group_id": id,
        "messages": messages_json(&db, &id, &s)?,
    })))
}

/// POST /api/code/groups/:gid/stop-current
pub(crate) async fn groups_stop_current(
    State(s): State<Arc<UiState>>,
    AxPath(gid): AxPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    load_session(&db, &gid)?;
    if let Some(api) = &s.agent_api {
        api.stop_agent(&gid);
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ===== Edit-format statistics =====

/// GET /api/code/edit-stats — how often the Edit tool applied exactly, needed
/// a fuzzy match, used a patch, or failed since the daemon started. The number
/// to watch when choosing a model's edit format.
pub(crate) async fn edit_stats() -> Json<serde_json::Value> {
    Json(crate::tools::edit::edit_stats_json())
}

// ===== Folder picker =====

#[derive(Deserialize, Default)]
pub(crate) struct LsQuery {
    #[serde(default)]
    path: Option<String>,
}

/// GET /api/fs/ls?path= — directories only, for the mobile folder picker.
/// Defaults to the home directory. Hidden and dependency folders are skipped.
pub(crate) async fn fs_ls(Query(q): Query<LsQuery>) -> Result<Json<serde_json::Value>, AppError> {
    let dir = match q.path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => expand_tilde(p),
        None => dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
    };
    if !dir.is_absolute() {
        return Err(bad("path must be absolute"));
    }
    if !dir.is_dir() {
        return Err(AppError(StatusCode::NOT_FOUND, "directory not found".into()));
    }
    let rd = std::fs::read_dir(&dir).map_err(|e| bad(format!("cannot read directory: {e}")))?;
    let mut dirs: Vec<serde_json::Value> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                return None;
            }
            Some(serde_json::json!({ "name": name, "path": e.path().to_string_lossy() }))
        })
        .collect();
    dirs.sort_by(|a, b| {
        a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    let parent = dir.parent().map(|p| p.to_string_lossy().to_string());
    Ok(Json(serde_json::json!({
        "current": dir.to_string_lossy(),
        "parent": parent,
        "dirs": dirs,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_refuses_escapes() {
        let root = Path::new("/w");
        assert!(safe_join(root, "../x").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
        assert!(safe_join(root, "a/../../b").is_err());
        assert_eq!(safe_join(root, "./src/main.rs").ok(), Some(PathBuf::from("/w/src/main.rs")));
    }

    #[test]
    fn language_guess_counts_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "").unwrap();
        std::fs::write(tmp.path().join("c.py"), "").unwrap();
        assert_eq!(guess_language(tmp.path()).as_deref(), Some("rust"));
        assert_eq!(guess_language(&tmp.path().join("nope")), None);
    }

    #[test]
    fn tree_skips_dependency_dirs_and_respects_budget() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/x")).unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        std::fs::write(tmp.path().join("README.md"), "").unwrap();
        let mut budget = 100;
        let t = tree(tmp.path(), "", 0, &mut budget);
        let names: Vec<_> = t.iter().map(|n| n["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, vec!["src", "README.md"], "dirs first, node_modules skipped");
        assert_eq!(t[0]["children"][0]["path"], "src/main.rs");
        let mut budget = 1;
        let t = tree(tmp.path(), "", 0, &mut budget);
        assert_eq!(t.len(), 1);
    }
}
