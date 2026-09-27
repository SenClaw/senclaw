//! `/api/chats/:jid/checkpoints*` — the person's view of what an agent changed,
//! step by step, and the button that puts a file back.
//!
//! Everything here reads the shadow repo through
//! [`crate::checkpoints::CheckpointService`]; the project's own `.git` is
//! never consulted or modified.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use super::core::{AppError, UiState};

fn svc(s: &Arc<UiState>) -> Result<Arc<crate::checkpoints::CheckpointService>, AppError> {
    s.checkpoints
        .clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "checkpoints_unset".into()))
}

fn internal(e: anyhow::Error) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// GET /api/chats/:jid/checkpoints
pub(crate) async fn checkpoints_list(
    State(s): State<Arc<UiState>>,
    Path(jid): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let svc = svc(&s)?;
    let items = svc.list(&jid).map_err(internal)?;
    let workspace = items.first().map(|c| c.workspace.clone());
    Ok(Json(serde_json::json!({
        "enabled": svc.is_enabled(&jid),
        "workspace": workspace,
        "items": items,
    })))
}

#[derive(Deserialize)]
pub(crate) struct SettingsBody {
    enabled: bool,
}

/// PUT /api/chats/:jid/checkpoints/settings
pub(crate) async fn checkpoints_settings(
    State(s): State<Arc<UiState>>,
    Path(jid): Path<String>,
    Json(body): Json<SettingsBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let svc = svc(&s)?;
    svc.set_enabled(&jid, body.enabled).map_err(internal)?;
    Ok(Json(serde_json::json!({ "ok": true, "enabled": body.enabled })))
}

#[derive(Deserialize, Default)]
pub(crate) struct DiffQuery {
    /// Another checkpoint id to diff against. Default: the parent commit.
    from: Option<i64>,
}

/// GET /api/chats/:jid/checkpoints/:id/diff?from=<id>
pub(crate) async fn checkpoint_diff(
    State(s): State<Arc<UiState>>,
    Path((jid, id)): Path<(String, i64)>,
    Query(q): Query<DiffQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let svc = svc(&s)?;
    let (cp, from_sha, files, diff, truncated) = svc
        .diff(&jid, id, q.from)
        .await
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "checkpoint": cp,
        "fromSha": from_sha,
        "files": files,
        "diff": diff,
        "truncated": truncated,
    })))
}

#[derive(Deserialize, Default)]
pub(crate) struct RestoreBody {
    /// Paths (relative to the workspace) to restore. Empty = whole tree.
    #[serde(default)]
    files: Vec<String>,
}

/// POST /api/chats/:jid/checkpoints/:id/restore
pub(crate) async fn checkpoint_restore(
    State(s): State<Arc<UiState>>,
    Path((jid, id)): Path<(String, i64)>,
    body: Option<Json<RestoreBody>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let svc = svc(&s)?;
    let files = body.map(|b| b.0.files).unwrap_or_default();
    let (report, new_cp) = svc
        .restore(&jid, id, &files)
        .await
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    if let (Some(cp), Some(api)) = (&new_cp, &s.agent_api) {
        api.broadcast_checkpoint_new(&jid, cp);
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "restored": report.restored,
        "removed": report.removed,
        "checkpoint": new_cp,
    })))
}

#[derive(Deserialize, Default)]
pub(crate) struct ExplainBody {
    #[serde(default)]
    from: Option<i64>,
    /// LLM profile id; default profile when absent.
    #[serde(default)]
    profile: Option<String>,
    /// `vi` / `en` / … — the answer's language. Default: Vietnamese.
    #[serde(default)]
    language: Option<String>,
}

const EXPLAIN_SYSTEM: &str = "You are reviewing a code change made by an AI agent on behalf of a person \
who has to decide whether to keep it. Explain, from the unified diff only:\n\
1. What changed, file by file, in plain language.\n\
2. Why it was probably done (infer from the change; say when you are unsure).\n\
3. Anything risky: behaviour changes, removed checks, hardcoded values, secrets, \
missing tests.\n\
Be concrete and short. Do not restate the diff. Do not invent files or lines that \
are not in it.";

/// POST /api/chats/:jid/checkpoints/:id/explain — Cline's "Explain changes":
/// one LLM call over the diff, no tools, no memory.
pub(crate) async fn checkpoint_explain(
    State(s): State<Arc<UiState>>,
    Path((jid, id)): Path<(String, i64)>,
    body: Option<Json<ExplainBody>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let svc = svc(&s)?;
    let body = body.map(|b| b.0).unwrap_or_default();
    let (cp, _from, files, diff, truncated) = svc
        .diff(&jid, id, body.from)
        .await
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    if diff.trim().is_empty() {
        return Ok(Json(serde_json::json!({
            "ok": true,
            "text": "",
            "files": files,
            "note": "no changes in this checkpoint",
        })));
    }
    let lang = body.language.as_deref().unwrap_or("vi");
    let system = format!("{EXPLAIN_SYSTEM}\nAnswer in language: {lang}.");
    let user = format!(
        "Checkpoint #{} — {} ({}){}\n\n```diff\n{}\n```",
        cp.id,
        cp.tool_name,
        cp.summary,
        if truncated { "\n(diff truncated)" } else { "" },
        diff
    );
    let result = super::llm_config::chat_completion(
        &s.config.paths.global_config_path,
        body.profile.as_deref(),
        &system,
        &user,
        2048,
        None,
    )
    .await
    .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e))?;
    super::llm_config::record_completion(
        &s.usage_recorder,
        "checkpoint:explain",
        &jid,
        &result,
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "text": result.text,
        "model": result.model,
        "files": files,
        "truncated": truncated,
    })))
}

#[derive(Deserialize, Default)]
pub(crate) struct DocumentBody {
    #[serde(default)]
    from: Option<i64>,
    #[serde(default)]
    profile: Option<String>,
    /// Overrides the title taken from the checkpoint summary.
    #[serde(default)]
    title: Option<String>,
    /// `vi` / `en`. Absent means: match whatever the project's docs already
    /// use, which is nearly always the right answer.
    #[serde(default)]
    language: Option<String>,
}

const DOCUMENT_SYSTEM: &str = "You are writing a short record of one code change for the \
project's own documentation, from a unified diff.\n\
Write markdown with these sections and nothing else:\n\
## Vì sao — the problem this change addresses, inferred from the diff. One paragraph.\n\
## Đã làm gì — what the change does, in plain language, grouped by concern rather than by file.\n\
## Cần chú ý — behaviour changes, removed checks, anything that could fail silently. \
Omit the section entirely if the diff shows none.\n\
Rules: do not restate the diff line by line. Do not list the files — they are \
already listed above your text. Do not invent files, tests, or verification that \
the diff does not show. Say \"không rõ từ diff\" when you cannot tell.";

/// POST /api/chats/:jid/checkpoints/:id/document — turn a checkpoint into a
/// markdown page under the project's `docs/changes/`.
///
/// `checkpoint_explain` already narrates a diff, but only into the chat, where
/// it scrolls away. This is the same diff with a durable destination.
///
/// The two halves of the page are kept apart on purpose: the file list comes
/// from the diff and is a fact, while the prose is a model reading that diff
/// and is inference. A page that blurs them invites the reader to trust the
/// guess as much as the measurement.
pub(crate) async fn checkpoint_document(
    State(s): State<Arc<UiState>>,
    Path((jid, id)): Path<(String, i64)>,
    body: Option<Json<DocumentBody>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let svc = svc(&s)?;
    let body = body.map(|b| b.0).unwrap_or_default();
    let (cp, _from, files, diff, truncated) = svc
        .diff(&jid, id, body.from)
        .await
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    if diff.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "this checkpoint changed nothing, so there is nothing to document".into(),
        ));
    }

    let store = crate::docs::DocsStore::for_working_dir(&cp.workspace);
    if !store.exists() {
        // Creating a docs/ tree inside somebody's repository is their call.
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!(
                "{} has no docs/ directory yet — create it first if this project should keep documentation here",
                cp.workspace
            ),
        ));
    }

    let lang = body.language.clone().unwrap_or_else(|| {
        match store.language() {
            crate::docs::DocsLanguage::English => "en".to_string(),
            // Vietnamese for a Vietnamese project, and for a project with no
            // documents yet: this daemon's own users write Vietnamese.
            _ => "vi".to_string(),
        }
    });
    let title = body
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| {
            if cp.summary.trim().is_empty() {
                format!("Thay đổi #{}", cp.id)
            } else {
                cp.summary.clone()
            }
        });

    let system = format!("{DOCUMENT_SYSTEM}\nWrite in language: {lang}.");
    let user = format!(
        "Checkpoint #{} — {} ({}){}\n\n```diff\n{}\n```",
        cp.id,
        cp.tool_name,
        cp.summary,
        if truncated { "\n(diff truncated)" } else { "" },
        diff
    );
    let result = super::llm_config::chat_completion(
        &s.config.paths.global_config_path,
        body.profile.as_deref(),
        &system,
        &user,
        2048,
        None,
    )
    .await
    .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e))?;
    super::llm_config::record_completion(&s.usage_recorder, "checkpoint:document", &jid, &result);

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let rel = format!("changes/{date}-{}.md", crate::docs::slugify(&title));
    let page = render_change_record(&title, &date, &cp, &files, truncated, &result.text);

    // A second change on the same subject the same day must not silently
    // replace the first: number it instead.
    let mut path = rel.clone();
    let mut attempt = 2;
    let out = loop {
        match store.write(&path, &page, false) {
            Ok(out) => break out,
            Err(_) if attempt <= 20 => {
                path = rel.replace(".md", &format!("-{attempt}.md"));
                attempt += 1;
            }
            Err(e) => return Err(AppError(StatusCode::BAD_REQUEST, format!("{e:#}"))),
        }
    };
    let index = store.rebuild_index().ok().flatten();

    Ok(Json(serde_json::json!({
        "ok": true,
        "path": out.absolute_path.to_string_lossy(),
        "relativePath": out.relative_path,
        "index": index.map(|p| p.to_string_lossy().to_string()),
        "model": result.model,
        "files": files,
        "truncated": truncated,
    })))
}

/// Assemble the page. The file list and the checkpoint metadata are written
/// here, from the diff; only the middle section is the model's.
fn render_change_record(
    title: &str,
    date: &str,
    cp: &crate::types::ChatCheckpoint,
    files: &[crate::checkpoints::shadow_repo::ChangedFile],
    truncated: bool,
    prose: &str,
) -> String {
    let mut out = format!("# {title}\n\n");
    out.push_str(&format!(
        "**Trạng thái:** đã thực hiện {date} · checkpoint #{} (`{}`)\n\n",
        cp.id,
        cp.sha.chars().take(8).collect::<String>()
    ));
    out.push_str("## File đã đổi\n\n");
    if files.is_empty() {
        out.push_str("_(diff không liệt kê file nào)_\n");
    } else {
        for f in files {
            // The status letter matters: "deleted" and "modified" are not the
            // same fact, and a reader chasing the change needs to know which.
            let what = match f.status.chars().next() {
                Some('A') => "thêm",
                Some('D') => "xoá",
                Some('R') => "đổi tên",
                _ => "sửa",
            };
            out.push_str(&format!("- `{}` ({what})\n", f.path));
        }
    }
    if truncated {
        out.push_str("\n_Diff bị cắt bớt vì quá dài; phần dưới đọc từ đoạn đã cắt._\n");
    }
    out.push_str(
        "\n> Danh sách trên lấy thẳng từ diff. Phần dưới là mô hình đọc diff suy ra,\n\
         > chưa ai kiểm chứng.\n\n",
    );
    out.push_str(prose.trim());
    out.push('\n');
    out
}
