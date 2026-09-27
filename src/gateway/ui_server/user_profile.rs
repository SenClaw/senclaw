//! Soul Core REST surface — `USER.md`, `TOOLS.md`, `AGENTS.md`.
//!
//! Sibling of [`super::profile_files`], which edits the *agent's* `SOUL.md`
//! per folder. These routes edit the three global files that describe the
//! **human** and the machine, and they live at `/api/user-profile`,
//! `/api/tools-notes` and `/api/agents-rules` rather than under
//! `/api/space/apps/` — anything under that prefix is gated per app id by
//! `app_auth`, which would be the wrong boundary here.

use std::sync::Arc;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};

use crate::user_profile::{self, Tier, UserProfile};

use super::core::{AppError, UiState};

// ===== Wire types =====

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct FieldDto {
    pub key: String,
    pub value: String,
    /// `"public"` | `"private"`.
    pub tier: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct DirectiveDto {
    pub text: String,
    pub observed: String,
    pub status: String,
    pub tier: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct UserProfileDto {
    pub fields: Vec<FieldDto>,
    pub directives: Vec<DirectiveDto>,
    pub notes: String,
    /// Absolute path, so the UI can tell the user where to find the file.
    pub path: String,
    /// Rendered preview of what a private chat would actually receive. The
    /// point of showing it is that the tier rule is invisible otherwise —
    /// the user cannot otherwise tell what the model sees.
    pub preview_full: Option<String>,
    /// Same, for a group chat. The two side by side are what make "public vs
    /// private" concrete.
    pub preview_public: Option<String>,
}

fn tier_str(t: Tier) -> String {
    match t {
        Tier::Public => "public".into(),
        Tier::Private => "private".into(),
    }
}

fn tier_from(s: &str) -> Tier {
    if s.eq_ignore_ascii_case("public") {
        Tier::Public
    } else {
        Tier::Private
    }
}

fn to_dto(p: &UserProfile, path: String) -> UserProfileDto {
    UserProfileDto {
        fields: p
            .fields
            .iter()
            .map(|f| FieldDto {
                key: f.key.clone(),
                value: f.value.clone(),
                tier: tier_str(f.tier),
            })
            .collect(),
        directives: p
            .directives
            .iter()
            .map(|d| DirectiveDto {
                text: d.text.clone(),
                observed: d.observed.clone(),
                status: match d.status {
                    crate::user_profile::DirectiveStatus::Active => "active".into(),
                    crate::user_profile::DirectiveStatus::Superseded => "superseded".into(),
                },
                tier: tier_str(d.tier),
            })
            .collect(),
        notes: p.notes.clone(),
        path,
        preview_full: user_profile::render(p, user_profile::ProfileScope::Full),
        preview_public: user_profile::render(p, user_profile::ProfileScope::PublicOnly),
    }
}

// ===== USER.md =====

/// GET /api/user-profile
pub(crate) async fn get_user_profile(State(s): State<Arc<UiState>>) -> Json<UserProfileDto> {
    let path = &s.config.paths.user_profile_path;
    let p = user_profile::get_or_load(path);
    Json(to_dto(&p, path.to_string_lossy().into_owned()))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PutUserProfile {
    /// Full replacement of the field list. Absent leaves fields untouched.
    pub fields: Option<Vec<FieldDto>>,
    pub notes: Option<String>,
}

/// PUT /api/user-profile
///
/// Fields only — directives are managed by the agent through the MCP tool so
/// their `observed` dates and supersede chain stay coherent. A form that let
/// the user hand-edit them would produce two contradictory actives, which is
/// the exact failure the status field exists to prevent.
pub(crate) async fn put_user_profile(
    State(s): State<Arc<UiState>>,
    Json(body): Json<PutUserProfile>,
) -> Result<Json<UserProfileDto>, AppError> {
    let path = s.config.paths.user_profile_path.clone();
    let mut p = user_profile::get_or_load(&path);

    if let Some(fields) = body.fields {
        p.fields = fields
            .into_iter()
            .filter(|f| !f.key.trim().is_empty())
            .map(|f| crate::user_profile::Field {
                key: f.key.trim().to_string(),
                value: f.value.trim().to_string(),
                tier: tier_from(&f.tier),
            })
            .collect();
    }
    if let Some(notes) = body.notes {
        p.notes = notes;
    }

    user_profile::save(&path, &p)
        .map_err(|e| AppError(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    notify_changed(&s, "user-profile");
    let fresh = user_profile::get_or_load(&path);
    Ok(Json(to_dto(&fresh, path.to_string_lossy().into_owned())))
}

// ===== Flat files (TOOLS.md, AGENTS.md) =====

#[derive(Debug, Serialize)]
pub(crate) struct FlatFileDto {
    pub content: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PutFlatFile {
    pub content: String,
}

/// GET /api/tools-notes
pub(crate) async fn get_tools_notes(State(s): State<Arc<UiState>>) -> Json<FlatFileDto> {
    let path = &s.config.paths.tools_notes_path;
    Json(FlatFileDto {
        content: std::fs::read_to_string(path).unwrap_or_default(),
        path: path.to_string_lossy().into_owned(),
    })
}

/// PUT /api/tools-notes
pub(crate) async fn put_tools_notes(
    State(s): State<Arc<UiState>>,
    Json(body): Json<PutFlatFile>,
) -> Result<Json<FlatFileDto>, AppError> {
    let path = s.config.paths.tools_notes_path.clone();
    user_profile::write_flat_file(&path, &body.content)
        .map_err(|e| AppError(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    notify_changed(&s, "tools-notes");
    Ok(Json(FlatFileDto {
        content: body.content,
        path: path.to_string_lossy().into_owned(),
    }))
}

/// GET /api/agents-rules
pub(crate) async fn get_agents_rules(State(s): State<Arc<UiState>>) -> Json<FlatFileDto> {
    let path = &s.config.paths.agents_rules_path;
    Json(FlatFileDto {
        content: std::fs::read_to_string(path).unwrap_or_default(),
        path: path.to_string_lossy().into_owned(),
    })
}

/// PUT /api/agents-rules
pub(crate) async fn put_agents_rules(
    State(s): State<Arc<UiState>>,
    Json(body): Json<PutFlatFile>,
) -> Result<Json<FlatFileDto>, AppError> {
    let path = s.config.paths.agents_rules_path.clone();
    user_profile::write_flat_file(&path, &body.content)
        .map_err(|e| AppError(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    notify_changed(&s, "agents-rules");
    Ok(Json(FlatFileDto {
        content: body.content,
        path: path.to_string_lossy().into_owned(),
    }))
}

// ===== Change notification =====

/// Tell connected UIs that one of the files changed.
///
/// The payload carries **only which file** — never its contents. This goes to
/// every admin socket, and `USER.md` may hold an email and a home address;
/// broadcasting the body would push private data down a channel that has a
/// wider audience than the file's own tier rule allows. Clients re-`GET`.
fn notify_changed(s: &Arc<UiState>, what: &str) {
    user_profile::invalidate();
    if let Some(api) = &s.agent_api {
        api.broadcast_event(serde_json::json!({
            "type": "user-profile:changed",
            "file": what,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_round_trips_through_the_wire_format() {
        assert_eq!(tier_from(&tier_str(Tier::Public)), Tier::Public);
        assert_eq!(tier_from(&tier_str(Tier::Private)), Tier::Private);
    }

    #[test]
    fn unknown_tier_string_is_private() {
        // A client sending garbage (or an older client sending nothing
        // meaningful) must not be able to make a field public.
        for s in ["", "PUBLIK", "yes", "true", "1"] {
            assert_eq!(tier_from(s), Tier::Private, "{s:?}");
        }
    }

    #[test]
    fn dto_previews_differ_between_scopes() {
        // The two previews are the UI's only way to show what the tier rule
        // does; if they came out identical the feature would look broken.
        let p = crate::user_profile::parse::parse("---\nname: A\nemail: a@b.c\n---\n");
        let dto = to_dto(&p, "/tmp/USER.md".into());
        let full = dto.preview_full.unwrap();
        let public = dto.preview_public.unwrap();
        assert!(full.contains("a@b.c"));
        assert!(!public.contains("a@b.c"));
    }
}
