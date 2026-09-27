//! `/api/patterns` — browse, run, import and sync Zen Patterns.
//!
//! | route | does |
//! |---|---|
//! | `GET  /api/patterns` | the resolved pattern list, plus sources and strategies |
//! | `GET  /api/patterns/:name` | one pattern's files, with the source it resolved to |
//! | `POST /api/patterns/run` | render and (unless `dryRun`) execute one pattern |
//! | `POST /api/patterns` | create or overwrite a pattern in a writable source |
//! | `POST /api/patterns/import` | upload a zip of pattern folders |
//! | `DELETE /api/patterns/:name` | delete from a writable source |
//! | `GET  /api/patterns/catalog` | sources we can install without being told a URL |
//! | `POST /api/patterns/catalog/:id/install` | install one of them |
//! | `GET/POST /api/patterns/sources` | list / add a source |
//! | `POST /api/patterns/sources/:id/sync` | clone or pull a git source |
//! | `POST /api/patterns/sources/:id/toggle` | enable or disable without deleting |
//! | `DELETE /api/patterns/sources/:id` | de-register and delete its files |
//!
//! Everything git touches runs inside `spawn_blocking`: `git2` is synchronous
//! and a Fabric clone is hundreds of files, which would otherwise park a Tokio
//! worker for the whole fetch.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::Json,
};
use axum_extra::extract::Multipart;
use serde::Deserialize;

use crate::patterns::{
    render_pattern, sync_source, PatternRegistry, PatternSource, PatternStore, RenderRequest,
    SourceKind, StoreError, USER_SOURCE_ID,
};

use super::core::{AppError, UiState};

fn bad(msg: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.into())
}

/// Map a store failure onto the status that describes it, so a UI can tell
/// "you typed a name that does not exist" from "that source is read-only".
fn store_err(e: StoreError) -> AppError {
    let status = match e {
        StoreError::NotFound(_) | StoreError::NoSource(_) => StatusCode::NOT_FOUND,
        StoreError::Exists(_) => StatusCode::CONFLICT,
        StoreError::ReadOnly(_) => StatusCode::FORBIDDEN,
        StoreError::BadName(_) => StatusCode::BAD_REQUEST,
        StoreError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    AppError(status, e.to_string())
}

fn store(s: &UiState) -> PatternStore {
    PatternStore::new(&s.config.paths.patterns_dir)
}

// ===== GET /api/patterns =====

#[derive(Debug, Deserialize)]
pub(crate) struct ListQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    source: String,
}

pub(crate) async fn patterns_list(
    State(s): State<Arc<UiState>>,
    Query(q): Query<ListQuery>,
) -> Json<serde_json::Value> {
    let st = store(&s);
    let reg = PatternRegistry::new(&st);
    let filter = (!q.source.trim().is_empty()).then(|| q.source.trim());
    let rows = reg.list(&q.q, filter);
    let counts = st.counts();

    // Sources travel with every list call because the UI shows them as the
    // grouping axis; a second round-trip for a handful of rows is not worth
    // the extra endpoint.
    let sources: Vec<serde_json::Value> = st
        .sources()
        .into_iter()
        .map(|src| {
            let count = counts.get(&src.id).copied().unwrap_or(0);
            let mut v = serde_json::to_value(&src).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("count".into(), count.into());
                obj.insert("writable".into(), src.writable().into());
            }
            v
        })
        .collect();

    Json(serde_json::json!({
        "patterns": rows,
        "sources": sources,
        "strategies": crate::patterns::strategy::list_strategies(&st.strategies_dir()),
    }))
}

// ===== GET /api/patterns/:name =====

pub(crate) async fn patterns_get(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let st = store(&s);
    let (src, files) = PatternRegistry::new(&st).resolve(&name).map_err(store_err)?;
    Ok(Json(serde_json::json!({
        "pattern": files,
        "source": src,
    })))
}

// ===== POST /api/patterns/run =====

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunBody {
    name: String,
    #[serde(default)]
    input: String,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    variables: BTreeMap<String, String>,
    /// `"auto"` follows the input's language; a language name pins it. Absent
    /// leaves the pattern's own wording in charge.
    #[serde(default)]
    language: Option<String>,
    /// LLM config id or label. Absent = the active model.
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    max_tokens: Option<u32>,
    /// Render only. The caller (usually the agent) then applies the prompt in
    /// its own turn, which costs no second LLM call.
    #[serde(default)]
    dry_run: bool,
}

/// Ceiling for a pattern run. Matches the Space-App bridge's cap: patterns
/// produce structured documents, and the failure mode of a low cap is a
/// summary that stops mid-sentence.
const MAX_OUTPUT_TOKENS: u32 = 32_000;
const DEFAULT_OUTPUT_TOKENS: u32 = 8_192;

pub(crate) async fn patterns_run(
    State(s): State<Arc<UiState>>,
    Json(body): Json<RunBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let st = store(&s);
    let (src, files) = PatternRegistry::new(&st)
        .resolve(&body.name)
        .map_err(store_err)?;

    let strategy = body
        .strategy
        .as_deref()
        .filter(|x| !x.trim().is_empty())
        .and_then(|n| crate::patterns::strategy::read_strategy(&st.strategies_dir(), n));

    let rendered = render_pattern(&RenderRequest {
        system: &files.system,
        user_template: files.user.as_deref(),
        input: &body.input,
        variables: body.variables.clone(),
        strategy: strategy.as_ref(),
        language: body.language.as_deref(),
    });

    if body.dry_run {
        return Ok(Json(serde_json::json!({
            "ok": true,
            "dryRun": true,
            "pattern": files.name,
            "source": src.id,
            "rendered": rendered,
        })));
    }

    if rendered.user.trim().is_empty() && !files.system.contains("{{input}}") {
        return Err(bad(
            "this pattern needs input text — paste the content to transform",
        ));
    }

    let max_tokens = body
        .max_tokens
        .unwrap_or(DEFAULT_OUTPUT_TOKENS)
        .min(MAX_OUTPUT_TOKENS);

    let result = super::llm_config::chat_completion(
        &s.config.paths.global_config_path,
        body.profile.as_deref(),
        &rendered.system,
        &rendered.user,
        max_tokens,
        None,
    )
    .await
    .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e))?;

    super::llm_config::record_completion(
        &s.usage_recorder,
        &format!("pattern:{}", files.name),
        "",
        &result,
    );

    Ok(Json(serde_json::json!({
        "ok": true,
        "pattern": files.name,
        "source": src.id,
        "text": result.text,
        "model": result.model,
        "finish": result.finish,
        "latencyMs": result.latency_ms,
        // Echoed so a caller can see exactly what was sent — a pattern that
        // misbehaves is almost always a placeholder that never got filled.
        "unresolved": rendered.unresolved,
    })))
}

// ===== POST /api/patterns =====

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveBody {
    name: String,
    system: String,
    #[serde(default)]
    user: Option<String>,
    /// Target source. Defaults to the user's own, which is the only one the UI
    /// offers for a hand-written pattern.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    overwrite: bool,
}

pub(crate) async fn patterns_save(
    State(s): State<Arc<UiState>>,
    Json(body): Json<SaveBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if body.system.trim().is_empty() {
        return Err(bad("a pattern needs a system prompt"));
    }
    let st = store(&s);
    let target = body.source.as_deref().unwrap_or(USER_SOURCE_ID);
    let src = st.source(target).map_err(store_err)?;
    // Seed the ledger on first write so the user source exists as a row even
    // before anything else touches sources.json.
    if target == USER_SOURCE_ID {
        let _ = st.upsert_source(src.clone());
    }
    let files = st
        .write(
            &src,
            &body.name,
            &body.system,
            body.user.as_deref(),
            body.overwrite,
        )
        .map_err(store_err)?;
    Ok(Json(serde_json::json!({ "ok": true, "pattern": files })))
}

// ===== DELETE /api/patterns/:name =====

#[derive(Debug, Deserialize)]
pub(crate) struct DeleteQuery {
    #[serde(default)]
    source: String,
}

pub(crate) async fn patterns_delete(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let st = store(&s);
    // Without an explicit source, delete the one the name actually resolves
    // to — deleting "whatever the user source happens to hold" would silently
    // do nothing when the visible pattern came from elsewhere.
    let src = if q.source.trim().is_empty() {
        PatternRegistry::new(&st)
            .resolve(&name)
            .map_err(store_err)?
            .0
    } else {
        st.source(q.source.trim()).map_err(store_err)?
    };
    st.delete(&src, &name).map_err(store_err)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ===== POST /api/patterns/import =====

/// A zip of pattern folders is capped well below the kit-bundle limit: this
/// path only ever carries `system.md`/`user.md` text.
const MAX_IMPORT_BYTES: usize = 32 * 1024 * 1024;

/// Import a zip whose entries are `<name>/system.md`, into a writable source.
///
/// Accepts the shape GitHub's "Download ZIP" produces (one wrapping folder)
/// as well as a bare folder-of-folders, because both are what a user actually
/// has in hand.
pub(crate) async fn patterns_import(
    State(s): State<Arc<UiState>>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut bytes: Option<Vec<u8>> = None;
    let mut target = USER_SOURCE_ID.to_string();
    let mut overwrite = false;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| bad(format!("bad upload: {e}")))?
    {
        match field.name().unwrap_or_default() {
            "file" => {
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| bad(format!("bad upload: {e}")))?;
                if data.len() > MAX_IMPORT_BYTES {
                    return Err(bad(format!(
                        "that zip is {} MB — the limit is {} MB",
                        data.len() / 1_048_576,
                        MAX_IMPORT_BYTES / 1_048_576
                    )));
                }
                bytes = Some(data.to_vec());
            }
            "source" => {
                if let Ok(v) = field.text().await {
                    if !v.trim().is_empty() {
                        target = v.trim().to_string();
                    }
                }
            }
            "overwrite" => {
                overwrite = field.text().await.map(|v| v == "true").unwrap_or(false);
            }
            _ => {}
        }
    }

    let bytes = bytes.ok_or_else(|| bad("no file uploaded"))?;
    let st = store(&s);
    let src = st.source(&target).map_err(store_err)?;
    if !src.writable() {
        return Err(store_err(StoreError::ReadOnly(src.id.clone())));
    }
    let _ = st.upsert_source(src.clone());

    // Unpack to a staging tree first, then hand it to the same `import_tree`
    // the kit installer uses — one implementation of "what counts as a
    // pattern". Staging lives under the patterns root rather than the system
    // temp dir so the rename into place never crosses a filesystem, and so a
    // crashed import leaves its debris somewhere the user can find it.
    let staging = st
        .root()
        .join(format!(".import-{:x}", rand::random::<u32>()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| store_err(StoreError::Io(e.to_string())))?;

    let result = (|| {
        let count = unzip_patterns(&bytes, &staging).map_err(bad)?;
        if count == 0 {
            return Err(bad(
                "no patterns found in that zip — expected folders each containing a system.md",
            ));
        }
        let written = st.import_tree(&src, &staging, overwrite).map_err(store_err)?;
        Ok((count, written))
    })();
    let _ = std::fs::remove_dir_all(&staging);
    let (count, written) = result?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "source": src.id,
        "found": count,
        "imported": written,
    })))
}

/// Extract `*/system.md` and `*/user.md` entries into `dest/<name>/`.
///
/// Only those two filenames are written, and each is placed under a single
/// sanitized directory component — a zip entry named `../../.ssh/authorized_keys`
/// therefore has nowhere to land. Returns how many patterns were unpacked.
fn unzip_patterns(bytes: &[u8], dest: &std::path::Path) -> Result<usize, String> {
    use std::io::{Cursor, Read};

    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not a zip file: {e}"))?;
    let mut found = std::collections::BTreeSet::new();

    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.is_dir() {
            continue;
        }
        let full = entry.name().replace('\\', "/");
        let parts: Vec<&str> = full.split('/').filter(|p| !p.is_empty()).collect();
        let Some(file) = parts.last() else { continue };
        if *file != "system.md" && *file != "user.md" {
            continue;
        }
        // The folder immediately above the file is the pattern name; anything
        // higher is the archive's own wrapping (GitHub's `repo-main/`).
        let Some(raw_dir) = parts.get(parts.len().wrapping_sub(2)) else {
            continue;
        };
        let Ok(name) = crate::patterns::sanitize_name(raw_dir) else {
            continue;
        };
        let mut body = String::new();
        if entry.read_to_string(&mut body).is_err() {
            continue;
        }
        let dir = dest.join(&name);
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        if std::fs::write(dir.join(file), body).is_ok() && *file == "system.md" {
            found.insert(name);
        }
    }
    Ok(found.len())
}

// ===== catalog =====

/// What the "add a source" screen can offer before the user types anything.
///
/// A blank five-field form is only fillable by someone who has already read the
/// target repository's layout; this is the list that makes the common case one
/// tap. See [`crate::patterns::catalog`].
pub(crate) async fn catalog_list(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "catalog": crate::patterns::catalog::entries(&store(&s)),
    }))
}

pub(crate) async fn catalog_install(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let st = store(&s);

    // The bundled set installs with no network at all — that is the whole
    // reason it is compiled in rather than fetched.
    if id == crate::patterns::STARTER_SOURCE_ID {
        let written = crate::patterns::install_starters(&st).map_err(store_err)?;
        return Ok(Json(serde_json::json!({
            "ok": true,
            "source": crate::patterns::STARTER_SOURCE_ID,
            "installed": written,
        })));
    }

    let preset = crate::patterns::catalog::git_preset(&id)
        .ok_or_else(|| bad(format!("no catalog entry \"{id}\"")))?;

    // Never-overwrite, same as everywhere else: a source the user already
    // added may point at their own fork.
    if st.source(preset.id).is_ok() {
        return Err(store_err(StoreError::Exists(preset.id.to_string())));
    }
    st.upsert_source(PatternSource {
        id: preset.id.to_string(),
        name: preset.name.to_string(),
        kind: SourceKind::Git,
        url: Some(preset.url.to_string()),
        git_ref: preset.git_ref.to_string(),
        subdir: preset.subdir.to_string(),
        strategies_subdir: preset.strategies_subdir.map(str::to_owned),
        enabled: true,
        installed_by: Some("catalog".to_string()),
        last_synced_at: None,
        last_error: None,
    })
    .map_err(store_err)?;

    let outcome = blocking_sync(&s, preset.id).await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "source": preset.id,
        "sync": outcome,
    })))
}

// ===== sources =====

pub(crate) async fn sources_list(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let st = store(&s);
    let counts = st.counts();
    let sources: Vec<serde_json::Value> = st
        .sources()
        .into_iter()
        .map(|src| {
            let mut v = serde_json::to_value(&src).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "count".into(),
                    counts.get(&src.id).copied().unwrap_or(0).into(),
                );
                obj.insert("writable".into(), src.writable().into());
            }
            v
        })
        .collect();
    Json(serde_json::json!({ "sources": sources }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AddSourceBody {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    url: String,
    #[serde(default, alias = "ref")]
    git_ref: String,
    #[serde(default)]
    subdir: String,
    #[serde(default)]
    strategies_subdir: Option<String>,
    /// Clone right away. Default true — a registered-but-empty source looks
    /// broken to whoever just added it.
    #[serde(default = "yes")]
    sync: bool,
}

fn yes() -> bool {
    true
}

pub(crate) async fn sources_add(
    State(s): State<Arc<UiState>>,
    Json(body): Json<AddSourceBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let url = body.url.trim().to_string();
    if url.is_empty() {
        return Err(bad("a git URL is required"));
    }
    // Derive the id from the repo name when the caller did not pick one.
    let raw_id = if body.id.trim().is_empty() {
        url.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("patterns")
            .trim_end_matches(".git")
            .to_string()
    } else {
        body.id.clone()
    };
    let id = crate::patterns::sanitize_name(&raw_id).map_err(store_err)?;

    let st = store(&s);
    if st.source(&id).is_ok() {
        return Err(store_err(StoreError::Exists(id)));
    }

    let src = PatternSource {
        id: id.clone(),
        name: if body.name.trim().is_empty() {
            id.clone()
        } else {
            body.name.trim().to_string()
        },
        kind: SourceKind::Git,
        url: Some(url),
        git_ref: if body.git_ref.trim().is_empty() {
            "main".to_string()
        } else {
            body.git_ref.trim().to_string()
        },
        subdir: body.subdir.trim().to_string(),
        strategies_subdir: body.strategies_subdir.clone(),
        enabled: true,
        installed_by: None,
        last_synced_at: None,
        last_error: None,
    };
    st.upsert_source(src).map_err(store_err)?;

    if !body.sync {
        return Ok(Json(serde_json::json!({ "ok": true, "source": id })));
    }
    let outcome = blocking_sync(&s, &id).await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "source": id,
        "sync": outcome,
    })))
}

pub(crate) async fn sources_sync(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let outcome = blocking_sync(&s, &id).await?;
    Ok(Json(serde_json::json!({ "ok": true, "sync": outcome })))
}

pub(crate) async fn sources_toggle(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let st = store(&s);
    let mut src = st.source(&id).map_err(store_err)?;
    src.enabled = !src.enabled;
    let enabled = src.enabled;
    st.upsert_source(src).map_err(store_err)?;
    Ok(Json(serde_json::json!({ "ok": true, "enabled": enabled })))
}

pub(crate) async fn sources_delete(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    store(&s).remove_source(&id).map_err(store_err)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Run one sync off the async runtime.
///
/// `git2` is blocking and a Fabric checkout is hundreds of files; leaving it
/// on a Tokio worker stalls every other request served by that thread for the
/// length of the fetch.
pub(crate) async fn blocking_sync(
    s: &Arc<UiState>,
    id: &str,
) -> Result<crate::patterns::SourceSyncOutcome, AppError> {
    let dir = s.config.paths.patterns_dir.clone();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || sync_source(&PatternStore::new(&dir), &id))
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(store_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_of(entries: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            for (name, body) in entries {
                w.start_file::<_, ()>(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                w.write_all(body.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn unzip_takes_pattern_folders_and_strips_a_github_wrapper() {
        let bytes = zip_of(&[
            ("fabric-main/summarize/system.md", "# H\n\nSummarise."),
            ("fabric-main/summarize/user.md", "template"),
            ("fabric-main/extract_wisdom/system.md", "# H\n\nExtract."),
            ("fabric-main/README.md", "ignored"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unzip_patterns(&bytes, dir.path()).unwrap(), 2);
        assert!(dir.path().join("summarize/system.md").is_file());
        assert!(dir.path().join("summarize/user.md").is_file());
        assert!(dir.path().join("extract_wisdom/system.md").is_file());
        assert!(!dir.path().join("README.md").exists());
    }

    #[test]
    fn unzip_cannot_be_walked_out_of_the_destination() {
        // The entry name is hostile in two ways at once: traversal segments,
        // and an absolute path. Both must collapse to one safe folder.
        let bytes = zip_of(&[
            ("../../../../etc/cron.d/system.md", "pwned"),
            ("/tmp/evil/system.md", "pwned"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        unzip_patterns(&bytes, dir.path()).unwrap();
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let p = entry.unwrap().path();
            assert!(p.starts_with(dir.path()), "escaped to {p:?}");
        }
        assert!(!std::path::Path::new("/tmp/evil/system.md").exists());
    }

    #[test]
    fn a_zip_with_no_patterns_reports_zero_rather_than_erroring() {
        let bytes = zip_of(&[("notes.txt", "hi"), ("docs/readme.md", "hi")]);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unzip_patterns(&bytes, dir.path()).unwrap(), 0);
    }

    #[test]
    fn a_non_zip_upload_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        assert!(unzip_patterns(b"definitely not a zip", dir.path()).is_err());
    }
}
