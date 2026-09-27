use std::fs;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, StatusCode},
    response::{Json, Response},
};
use serde::Deserialize;

use crate::subagents::disabled::{
    disable_subagent, enable_subagent, is_subagent_disabled, read_disabled_subagents,
};

use super::core::{AppError, UiState};

// ===== /api/subagents =====

pub(crate) async fn subagents_list(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let personas: Vec<serde_json::Value> = if let Some(ref pr) = s.persona_registry {
        let mut reg = pr.lock().unwrap();
        reg.reload();
        let disabled = read_disabled_subagents();
        reg.list()
            .into_iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "description": p.description,
                    "tools": p.tools,
                    "model": p.model,
                    "maxConcurrent": p.max_concurrent,
                    "filePath": p.file_path,
                    "disabled": disabled.contains(&p.name),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Json(serde_json::json!({ "subagents": personas }))
}

// ===== /api/subagents/create =====

#[derive(Deserialize)]
pub(crate) struct SubagentCreateBody {
    name: String,
    content: String,
}

pub(crate) async fn subagents_create(
    State(s): State<Arc<UiState>>,
    Json(body): Json<SubagentCreateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if body.name.is_empty() || body.content.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "name and content are required".into(),
        ));
    }
    let filename = sanitize_persona_filename(&body.name);
    if filename.is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid name".into()));
    }

    let dir = &s.config.paths.virtual_agents_dir;
    let file_path = dir.join(format!("{filename}.md"));

    if file_path.exists() {
        return Err(AppError(
            StatusCode::CONFLICT,
            format!("A persona file \"{filename}.md\" already exists."),
        ));
    }

    // Check for duplicate names
    if let Some(ref pr) = s.persona_registry {
        let reg = pr.lock().unwrap();
        if reg.get(&body.name).is_some() {
            return Err(AppError(
                StatusCode::CONFLICT,
                format!("A persona named \"{}\" already exists.", body.name),
            ));
        }
    }

    fs::create_dir_all(dir)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    fs::write(&file_path, &body.content)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if let Some(ref pr) = s.persona_registry {
        pr.lock().unwrap().reload();
    }

    Ok(Json(
        serde_json::json!({ "ok": true, "filename": format!("{filename}.md") }),
    ))
}

pub(crate) fn sanitize_persona_filename(name: &str) -> String {
    name.trim()
        .replace(char::is_whitespace, "-")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || c.is_alphabetic())
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

// ===== /api/subagents/{name}/readme =====

pub(crate) async fn subagents_readme(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Response, AppError> {
    let persona = get_persona_file(&s, &name).await?;
    let content = fs::read_to_string(&persona.file_path).unwrap_or_default();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(content))
        .unwrap())
}

pub(crate) async fn subagents_readme_save(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
    body: String,
) -> Result<Json<serde_json::Value>, AppError> {
    let persona = get_persona_file(&s, &name).await?;
    fs::write(&persona.file_path, &body)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(ref pr) = s.persona_registry {
        pr.lock().unwrap().reload();
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Helper to find a persona by name, returning its PersonaConfig.
async fn get_persona_file(
    s: &UiState,
    name: &str,
) -> Result<crate::agent::persona_registry::PersonaConfig, AppError> {
    if let Some(ref pr) = s.persona_registry {
        let reg = pr.lock().unwrap();
        reg.get(name)
            .cloned()
            .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "Not found".into()))
    } else {
        Err(AppError(StatusCode::NOT_FOUND, "Not found".into()))
    }
}

// ===== /api/subagents/{name}/{enable|disable} =====

pub(crate) async fn subagents_toggle(
    State(s): State<Arc<UiState>>,
    AxumPath((name, action)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Verify persona exists
    if let Some(ref pr) = s.persona_registry {
        let reg = pr.lock().unwrap();
        if reg.get(&name).is_none() {
            return Err(AppError(StatusCode::NOT_FOUND, "Not found".into()));
        }
    }
    match action.as_str() {
        "enable" => enable_subagent(&name),
        "disable" => disable_subagent(&name),
        _ => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "action must be enable or disable".into(),
            ))
        }
    }
    let disabled = is_subagent_disabled(&name);
    Ok(Json(
        serde_json::json!({ "name": name, "disabled": disabled }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_persona_filename() {
        assert_eq!(sanitize_persona_filename("My Agent"), "My-Agent");
        assert_eq!(sanitize_persona_filename("hello world"), "hello-world");
        assert_eq!(sanitize_persona_filename("  spaces  "), "spaces");
        assert_eq!(sanitize_persona_filename("safe_name"), "safe_name");
        assert_eq!(sanitize_persona_filename("with-dash"), "with-dash");
    }
}
