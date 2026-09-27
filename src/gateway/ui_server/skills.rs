use std::collections::HashSet;
use std::fs;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{header, StatusCode},
    response::{Json, Response},
};
use serde::Deserialize;

use crate::clawhub::client::{download_skill_zip, get_skill_meta, search_skills, DEFAULT_REGISTRY};
use crate::clawhub::lockfile::{
    extract_zip_to_dir, read_lockfile, read_skill_origin, write_lockfile, write_skill_origin,
};
use crate::clawhub::signal::emit_skills_refresh;
use crate::skills::disabled::{
    disable_skill, enable_skill, is_skill_disabled, read_disabled_skills,
};
use crate::skills::scan::load_all_local_skills;

use super::core::{AppError, UiState};

// ===== /api/skills =====

pub(crate) async fn skills_list(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    // The skill scan is blocking filesystem I/O (walks every source dir and
    // parses SKILL.md frontmatter). Running it directly on the async executor
    // can starve other tasks and, if a single skill file panics during parse,
    // drop the HTTP connection before headers are sent ("Connection closed
    // before full header was received"). Offload to a blocking thread and turn
    // any panic into a proper 500 response instead.
    let config = Arc::clone(&s.config);
    let result = tokio::task::spawn_blocking(move || {
        let skills = load_all_local_skills(&config);
        let disabled = read_disabled_skills();
        skills
            .iter()
            .map(|sk| {
                serde_json::json!({
                    "name": sk.name,
                    "description": sk.description,
                    "version": sk.version,
                    "source": sk.source,
                    "dir": sk.dir,
                    "disabled": disabled.contains(&sk.name),
                    "triggers": sk.metadata.triggers,
                    "allowedTools": sk.metadata.allowed_tools,
                    "eligible": sk.eligible,
                })
            })
            .collect::<Vec<serde_json::Value>>()
    })
    .await
    .map_err(|e| {
        AppError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("skill scan failed: {e}"),
        )
    })?;
    Ok(Json(serde_json::json!({ "skills": result })))
}

// ===== /api/skills/remote-search =====

#[derive(Deserialize)]
pub(crate) struct RemoteSearchQuery {
    q: Option<String>,
}

pub(crate) async fn skills_remote_search(
    State(s): State<Arc<UiState>>,
    Query(q): Query<RemoteSearchQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let query = q.q.unwrap_or_default();
    if query.trim().is_empty() {
        return Ok(Json(serde_json::json!({ "results": [] })));
    }
    let registry = std::env::var("CLAWHUB_REGISTRY")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
    let raw = search_skills(&query, Some(&registry), Some(20), None)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let local_skills = load_all_local_skills(&s.config);
    let local_names: HashSet<&str> = local_skills.iter().map(|sk| sk.name.as_str()).collect();
    let results: Vec<serde_json::Value> = raw
        .into_iter()
        .map(|r| {
            let mut v = serde_json::to_value(&r).unwrap_or_default();
            v["installed"] = serde_json::Value::Bool(local_names.contains(r.slug.as_str()));
            v
        })
        .collect();
    Ok(Json(serde_json::json!({ "results": results })))
}

// ===== /api/skills/install =====

#[derive(Deserialize)]
pub(crate) struct SkillInstallBody {
    slug: String,
}

pub(crate) async fn skills_install(
    State(s): State<Arc<UiState>>,
    Json(body): Json<SkillInstallBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let slug = body.slug.trim().to_string();
    if slug.is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "slug required".into()));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(AppError(StatusCode::BAD_REQUEST, "invalid slug".into()));
    }
    let managed_dir = &s.config.paths.managed_skills_dir;
    let target = managed_dir.join(&slug);
    // Path traversal guard
    let canonical_managed = managed_dir
        .canonicalize()
        .unwrap_or_else(|_| managed_dir.clone());
    if !target
        .canonicalize()
        .unwrap_or_else(|_| target.clone())
        .starts_with(&canonical_managed)
        && target.exists()
    {
        return Err(AppError(StatusCode::BAD_REQUEST, "invalid slug".into()));
    }

    let registry = std::env::var("CLAWHUB_REGISTRY")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());

    let meta = get_skill_meta(&slug, Some(&registry), None)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?;

    if meta
        .moderation
        .as_ref()
        .map_or(false, |m| m.is_malware_blocked)
    {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            format!("{slug} is flagged as malicious"),
        ));
    }

    let version = meta
        .latest_version
        .as_ref()
        .map(|v| v.version.clone())
        .ok_or_else(|| {
            AppError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "no version available".into(),
            )
        })?;

    let zip_buf = download_skill_zip(&slug, &version, Some(&registry), None)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?;

    if target.exists() {
        let _ = tokio::fs::remove_dir_all(&target).await;
    }
    extract_zip_to_dir(&zip_buf, &target)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let _ = write_skill_origin(
        &target,
        &crate::clawhub::lockfile::SkillOrigin {
            version: 1,
            registry,
            slug: slug.clone(),
            installed_version: version.clone(),
            installed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        },
    );

    let mut lock = read_lockfile(managed_dir);
    lock.skills.insert(
        slug.clone(),
        crate::clawhub::lockfile::LockfileEntry {
            version: Some(version.clone()),
            installed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        },
    );
    let _ = write_lockfile(managed_dir, &lock);

    if let Some(ref api) = s.agent_api {
        api.reload_all_skills();
    }
    let _ = emit_skills_refresh(&s.config);

    Ok(Json(
        serde_json::json!({ "ok": true, "slug": slug, "version": version }),
    ))
}

// ===== DELETE /api/skills/{name} =====

pub(crate) async fn skills_uninstall(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let skills = load_all_local_skills(&s.config);
    let skill = skills
        .iter()
        .find(|sk| sk.name == name)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "Skill not found".into()))?;

    if skill.source != "clawhub-managed" {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "Only ClaWHub-managed skills can be uninstalled".into(),
        ));
    }

    let managed_dir = &s.config.paths.managed_skills_dir;
    let canonical_managed = managed_dir.canonicalize().map_err(|_| {
        AppError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Managed skills dir missing".into(),
        )
    })?;
    let canonical_skill = skill
        .dir
        .canonicalize()
        .map_err(|_| AppError(StatusCode::NOT_FOUND, "Skill directory not found".into()))?;
    if !canonical_skill.starts_with(&canonical_managed) {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Invalid skill path".into(),
        ));
    }

    let origin = read_skill_origin(&skill.dir);
    let slug = origin
        .as_ref()
        .map(|o| o.slug.clone())
        .or_else(|| {
            skill
                .dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "Invalid skill directory".into()))?;

    tokio::fs::remove_dir_all(&skill.dir)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut lock = read_lockfile(managed_dir);
    lock.skills.remove(&slug);
    lock.skills.remove(&name);
    let _ = write_lockfile(managed_dir, &lock);

    enable_skill(&name);

    if let Some(ref api) = s.agent_api {
        api.reload_all_skills();
    }
    let _ = emit_skills_refresh(&s.config);

    Ok(Json(
        serde_json::json!({ "ok": true, "name": name, "slug": slug }),
    ))
}

// ===== POST /api/skills/create =====

#[derive(Deserialize)]
pub(crate) struct SkillCreateBody {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    content: String,
    /// Keyword phrases that hint when the skill applies. Written into the
    /// SKILL.md frontmatter as a `triggers:` YAML list so the daemon can
    /// auto-surface the skill when a user prompt matches (see
    /// `zen_core::engine::match_skill_name`).
    #[serde(default)]
    triggers: Vec<String>,
    /// Overwrite an existing skill of the same name instead of 409-ing. Used by
    /// the Skill Builder app to iterate on a draft it just created.
    #[serde(default)]
    overwrite: bool,
}

/// YAML-quote a scalar string for a double-quoted flow scalar.
fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Assemble a SKILL.md from its parts: `name` + `description` (+ optional
/// `triggers` block sequence) frontmatter, then the markdown body. Kept separate
/// from the handler so it can be unit-tested. `triggers` are trimmed and empties
/// dropped; when none remain no `triggers:` key is emitted.
fn build_skill_md(slug: &str, description: &str, content: &str, triggers: &[String]) -> String {
    let desc = description.trim().replace(['\n', '\r'], " ");
    let mut frontmatter = format!("name: {slug}\ndescription: {desc}\n");
    let triggers: Vec<&String> = triggers.iter().filter(|t| !t.trim().is_empty()).collect();
    if !triggers.is_empty() {
        frontmatter.push_str("triggers:\n");
        for t in &triggers {
            frontmatter.push_str(&format!("  - {}\n", yaml_quote(t.trim())));
        }
    }
    let body_md = if content.trim().is_empty() {
        format!("# {slug}\n\nDescribe what this skill does and when to use it.\n")
    } else {
        format!("{}\n", content.trim())
    };
    format!("---\n{frontmatter}---\n\n{body_md}")
}

/// Create a new local skill: scaffolds `<managed_skills_dir>/<slug>/SKILL.md`
/// with YAML frontmatter (`name`, `description`, optional `triggers`) plus the
/// supplied body.
pub(crate) async fn skills_create(
    State(s): State<Arc<UiState>>,
    Json(body): Json<SkillCreateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let slug = body.name.trim().to_string();
    if slug.is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "name required".into()));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "name must be slug-like (letters, digits, - and _)".into(),
        ));
    }
    let managed_dir = &s.config.paths.managed_skills_dir;
    let target = managed_dir.join(&slug);
    if target.exists() && !body.overwrite {
        return Err(AppError(
            StatusCode::CONFLICT,
            "a skill with that name already exists".into(),
        ));
    }
    fs::create_dir_all(&target)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Triggers become a YAML block sequence so the scanner's `as_string_list`
    // picks each phrase up verbatim (see `zen_core::engine::match_skill_name`).
    let md = build_skill_md(&slug, &body.description, &body.content, &body.triggers);
    fs::write(target.join("SKILL.md"), md)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if let Some(ref api) = s.agent_api {
        api.reload_all_skills();
    }
    let _ = emit_skills_refresh(&s.config);

    Ok(Json(serde_json::json!({ "ok": true, "name": slug })))
}

// ===== /api/skills/{name}/readme =====

pub(crate) async fn skills_readme(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Response, AppError> {
    let skills = load_all_local_skills(&s.config);
    let skill = skills
        .iter()
        .find(|sk| sk.name == name)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "Not found".into()))?;
    let content = fs::read_to_string(&skill.file_path).unwrap_or_default();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(content))
        .unwrap())
}

pub(crate) async fn skills_readme_save(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
    body: String,
) -> Result<Json<serde_json::Value>, AppError> {
    let skills = load_all_local_skills(&s.config);
    let skill = skills
        .iter()
        .find(|sk| sk.name == name)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "Not found".into()))?;
    // Skills installed by a Space App are read-only — edit the app instead.
    if skill.source.starts_with("app:") {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "This skill was installed by a Space App and cannot be edited. Update the app instead."
                .into(),
        ));
    }
    fs::write(&skill.file_path, &body)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ===== /api/skills/{name}/{enable|disable} =====

pub(crate) async fn skills_toggle(
    State(s): State<Arc<UiState>>,
    AxumPath((name, action)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let skills = load_all_local_skills(&s.config);
    if !skills.iter().any(|sk| sk.name == name) {
        return Err(AppError(StatusCode::NOT_FOUND, "Skill not found".into()));
    }
    match action.as_str() {
        "enable" => enable_skill(&name),
        "disable" => disable_skill(&name),
        _ => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "action must be enable or disable".into(),
            ))
        }
    }
    if let Some(ref api) = s.agent_api {
        api.reload_all_skills();
    }
    let _ = emit_skills_refresh(&s.config);
    let disabled = is_skill_disabled(&name);
    Ok(Json(
        serde_json::json!({ "name": name, "disabled": disabled }),
    ))
}

#[cfg(test)]
mod tests {
    use super::build_skill_md;
    use crate::skills::metadata::parse_skill_metadata;

    #[test]
    fn writes_triggers_into_frontmatter_and_roundtrips() {
        let triggers = vec![
            "who calls this".to_string(),
            " find callers ".to_string(),     // trimmed
            "".to_string(),                   // dropped
            "ai gọi hàm \"này\"".to_string(), // quote-escaped
        ];
        let md = build_skill_md(
            "sb-selftest",
            "Use when auditing callers",
            "# Body\n\nStep 1.",
            &triggers,
        );

        assert!(md.starts_with("---\nname: sb-selftest\n"));
        assert!(md.contains("triggers:\n"));
        assert!(md.contains("  - \"who calls this\"\n"));
        assert!(md.contains("  - \"find callers\"\n")); // trimmed
        assert!(md.contains(r#"  - "ai gọi hàm \"này\"""#)); // escaped
        assert!(md.contains("\n---\n\n# Body"));

        // The frontmatter must parse back to exactly the non-empty triggers so
        // the trigger matcher can consume them.
        let meta = parse_skill_metadata(&md, "sb-selftest", "");
        assert_eq!(
            meta.triggers,
            vec!["who calls this", "find callers", "ai gọi hàm \"này\""]
        );
    }

    #[test]
    fn omits_triggers_key_when_none() {
        let md = build_skill_md("plain", "desc", "body", &[]);
        assert!(!md.contains("triggers:"));
        assert!(md.contains("name: plain\ndescription: desc\n---"));
    }
}
