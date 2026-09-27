//! Cowork team REST API — minimal multi-agent team management.
//!
//! A team is a (manager profile + member specialists + workspace) tuple.
//! Opening a team materialises it as a regular chat group keyed
//! `cowork:<team_id>` with `groupType="cowork"` and `folder=manager_folder`,
//! so the rest of the chat plumbing (engine, messages, history, broadcast)
//! works unchanged. The manager profile uses the existing dispatch MCP
//! tools to delegate to member specialists.

use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::cowork_tasks::CoworkTeamTask;
use crate::db::cowork_teams::{CoworkTeam, CoworkTeamSettings, TeamMember};
use crate::db::cowork_templates::CoworkTemplate;
use crate::types::GroupBinding;

/// Owned, source-agnostic view of a template (built-in or custom) used by the
/// instantiation flow.
struct ResolvedTemplate {
    name: String,
    manager: String,
    members: Vec<TeamMember>,
    settings: CoworkTeamSettings,
}
use crate::util::local_time::local_iso_string_now;

// ─── Built-in team templates ────────────────────────────────────────────────
//
// Pre-defined squad blueprints using the 8 builtin personas installed by
// `install_builtin_personas`. Each template names the manager + members
// the user can spin up with one click on the Cowork page.

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TemplateMemberSpec {
    pub folder: &'static str,
    pub role: &'static str,
    pub responsibilities: &'static str,
    /// Pre-built typed trigger JSON (Vec<TriggerRule>).
    pub triggers_json: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TeamTemplate {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub manager: &'static str,
    pub manager_role: &'static str,
    pub members: &'static [TemplateMemberSpec],
    pub icon: &'static str,
}

// Cowork-specific templates: each one is a DAG team — a lead that PLANS +
// DELEGATES + SYNTHESIZES, never doing the underlying work itself. Members
// are specialists with concrete trigger rules so the lead knows when to
// hand off. The templates are deliberately small (1 lead + 2-3 members) so
// the dispatch graph stays comprehensible.

const T_USER_MSG: &str = r#"[{"type":"task_assigned"}]"#;

const BUILTIN_TEMPLATES: &[TeamTemplate] = &[
    TeamTemplate {
        id: "research-bureau",
        name: "Research Bureau",
        description: "Lead delegates web research → fact-checking → digest. Final answer is a cited summary.",
        manager: "research-lead",
        manager_role: "lead",
        icon: "🔬",
        members: &[
            TemplateMemberSpec {
                folder: "web-scout",
                role: "scout",
                responsibilities: "Browse the web, fetch URLs, collect raw evidence.",
                triggers_json: T_USER_MSG,
            },
            TemplateMemberSpec {
                folder: "fact-checker",
                role: "verifier",
                responsibilities: "Cross-check claims against sources; flag uncited assertions.",
                triggers_json: T_USER_MSG,
            },
            TemplateMemberSpec {
                folder: "digest-writer",
                role: "synthesizer",
                responsibilities: "Compose the final summary with inline citations.",
                triggers_json: T_USER_MSG,
            },
        ],
    },
    TeamTemplate {
        id: "code-squad",
        name: "Code Squad",
        description: "Tech lead splits the request → writer implements → reviewer audits → tester verifies.",
        manager: "tech-lead",
        manager_role: "lead",
        icon: "⌨️",
        members: &[
            TemplateMemberSpec {
                folder: "code-writer",
                role: "implementer",
                responsibilities: "Implement the smallest change that satisfies the task.",
                triggers_json: T_USER_MSG,
            },
            TemplateMemberSpec {
                folder: "code-reviewer",
                role: "reviewer",
                responsibilities: "Audit the diff for correctness, security, and style.",
                triggers_json: T_USER_MSG,
            },
            TemplateMemberSpec {
                folder: "test-engineer",
                role: "verifier",
                responsibilities: "Add/extend tests; confirm the change is exercised.",
                triggers_json: T_USER_MSG,
            },
        ],
    },
    TeamTemplate {
        id: "writing-studio",
        name: "Writing Studio",
        description: "Editor coordinates research → drafting → proofreading. Output is publication-ready copy.",
        manager: "editor-in-chief",
        manager_role: "lead",
        icon: "✍️",
        members: &[
            TemplateMemberSpec {
                folder: "researcher",
                role: "researcher",
                responsibilities: "Gather sources and angles for the piece.",
                triggers_json: T_USER_MSG,
            },
            TemplateMemberSpec {
                folder: "drafter",
                role: "drafter",
                responsibilities: "Write the first draft based on research.",
                triggers_json: T_USER_MSG,
            },
            TemplateMemberSpec {
                folder: "proofreader",
                role: "proofreader",
                responsibilities: "Catch typos, grammar, voice inconsistencies.",
                triggers_json: T_USER_MSG,
            },
        ],
    },
    TeamTemplate {
        id: "product-council",
        name: "Product Council",
        description: "Product lead frames the call → UX, engineering, and data weigh in. Output is a single decision memo.",
        manager: "product-lead",
        manager_role: "lead",
        icon: "🎯",
        members: &[
            TemplateMemberSpec {
                folder: "ux-designer",
                role: "designer",
                responsibilities: "Argue the user-experience perspective.",
                triggers_json: T_USER_MSG,
            },
            TemplateMemberSpec {
                folder: "engineer-rep",
                role: "engineer",
                responsibilities: "Argue the feasibility / cost perspective.",
                triggers_json: T_USER_MSG,
            },
            TemplateMemberSpec {
                folder: "data-analyst",
                role: "analyst",
                responsibilities: "Argue the data / impact perspective.",
                triggers_json: T_USER_MSG,
            },
        ],
    },
    TeamTemplate {
        id: "solo-pro",
        name: "Solo Pro",
        description: "Just a general-purpose assistant — no dispatch, no delegation. Use when the task fits one mind.",
        manager: "general-assistant",
        manager_role: "solo",
        icon: "🤖",
        members: &[],
    },
];

use super::core::{AppError, UiState};

#[derive(Debug, Serialize)]
pub(crate) struct TeamView {
    pub id: String,
    pub name: String,
    pub manager_folder: String,
    pub members: Vec<TeamMember>,
    pub workspace_dir: Option<String>,
    pub created_at: String,
    /// jid of the auto-materialised chat group (always `cowork:<id>`).
    pub jid: String,
    pub settings: CoworkTeamSettings,
}

fn to_view(t: CoworkTeam) -> TeamView {
    let jid = format!("cowork:{}", t.id);
    TeamView {
        id: t.id,
        name: t.name,
        manager_folder: t.manager_folder,
        members: t.members,
        workspace_dir: t.workspace_dir,
        created_at: t.created_at,
        jid,
        settings: t.settings,
    }
}

fn db(s: &UiState) -> Result<Arc<crate::db::Db>, AppError> {
    s.db.clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))
}

/// GET /api/cowork/teams
pub(crate) async fn list_teams(
    State(s): State<Arc<UiState>>,
) -> Result<Json<Vec<TeamView>>, AppError> {
    let db = db(&s)?;
    let teams = db
        .list_cowork_teams()
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(teams.into_iter().map(to_view).collect()))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateTeamBody {
    pub name: String,
    pub manager_folder: String,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub workspace_dir: Option<String>,
}

/// POST /api/cowork/teams
///
/// Creates the team row AND materialises a corresponding chat group so
/// `cowork:<id>` is reachable immediately. Members must be agent folders
/// that already exist; the manager + member personas drive the chat.
pub(crate) async fn create_team(
    State(s): State<Arc<UiState>>,
    Json(body): Json<CreateTeamBody>,
) -> Result<Json<TeamView>, AppError> {
    if body.name.trim().is_empty() || body.manager_folder.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "name and manager_folder are required".into(),
        ));
    }
    let db = db(&s)?;

    // Helper: ensure agent profile exists for a folder slug.
    // If the agent isn't in DB, try to bootstrap from PersonaRegistry
    // (built-in or user-defined persona md). Returns true if usable.
    let ensure_agent = |slug: &str| -> bool {
        if slug.trim().is_empty() {
            return false;
        }
        if db
            .get_agent_by_folder(slug)
            .map(|o| o.is_some())
            .unwrap_or(false)
        {
            return true;
        }
        // Try to seed from persona file.
        let core_prompt = s
            .persona_registry
            .as_ref()
            .and_then(|r| {
                r.lock()
                    .ok()
                    .and_then(|g| g.get(slug).map(|p| p.system_prompt.clone()))
            })
            .unwrap_or_default();
        let now = local_iso_string_now();
        if db
            .insert_agent(slug, slug, false, None, None, &core_prompt, None, &now)
            .is_err()
        {
            return false;
        }
        crate::gateway::group_manager::ensure_agent_dirs(&s.config, slug, slug);
        if !core_prompt.trim().is_empty() {
            crate::gateway::group_manager::write_soul_md(&s.config, slug, slug, &core_prompt);
        }
        true
    };

    // Manager must resolve (existing agent OR known persona we can seed).
    if !ensure_agent(&body.manager_folder) {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!(
                "manager not found and no persona named {}",
                body.manager_folder
            ),
        ));
    }

    // Members are PERSONAS — independent of the Profile/agent table.
    // We DO NOT auto-create agent rows here. Instead we accept any
    // non-empty slug; if a persona file with that slug doesn't yet exist,
    // the user can author one via the cowork persona endpoint. The
    // dispatch path runs members via PersonaRegistry, not AgentPool.
    let mut members: Vec<TeamMember> = Vec::new();
    for m in body.members.iter() {
        let slug = m.trim();
        if !slug.is_empty() {
            members.push(TeamMember::from_folder(slug.to_string()));
        }
    }

    let id = Uuid::new_v4().to_string();
    let now = local_iso_string_now();
    let team = CoworkTeam {
        id: id.clone(),
        name: body.name.trim().to_string(),
        manager_folder: body.manager_folder.trim().to_string(),
        members: members.clone(),
        workspace_dir: body
            .workspace_dir
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        created_at: now.clone(),
        settings: Default::default(),
    };

    db.insert_cowork_team(&team)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;

    // Materialise the chat group so the team is immediately reachable.
    // Uses the existing group_manager.register path (creates SOUL.md /
    // MEMORY.md if missing for the manager folder, broadcasts to clients).
    let jid = format!("cowork:{id}");
    let binding = GroupBinding {
        jid: jid.clone(),
        folder: team.manager_folder.clone(),
        name: team.name.clone(),
        channel: String::new(),
        group_type: "cowork".to_string(),
        requires_trigger: false,
        allowed_tools: None,
        allowed_paths: None,
        allowed_work_dirs: team.workspace_dir.as_ref().map(|w| vec![w.clone()]),
        bot_token: None,
        max_messages: None,
        llm_config_id: None,
        last_active: None,
        added_at: now,
    };
    s.group_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "group_manager not wired".into(),
            )
        })?
        .register(&db, &s.config, &binding);

    Ok(Json(to_view(team)))
}

/// GET /api/cowork/templates
///
/// Unified template shape returned to the UI — covers both built-in blueprints
/// and user-authored custom templates. `builtin` templates are read-only.
#[derive(Debug, Serialize)]
pub(crate) struct TemplateView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub manager: String,
    pub manager_role: String,
    pub members: Vec<TeamMember>,
    pub settings: CoworkTeamSettings,
    pub builtin: bool,
}

fn builtin_to_view(t: &TeamTemplate) -> TemplateView {
    TemplateView {
        id: t.id.to_string(),
        name: t.name.to_string(),
        description: t.description.to_string(),
        icon: t.icon.to_string(),
        manager: t.manager.to_string(),
        manager_role: t.manager_role.to_string(),
        members: t
            .members
            .iter()
            .map(|m| TeamMember {
                folder: m.folder.to_string(),
                role: Some(m.role.to_string()),
                responsibilities: Some(m.responsibilities.to_string()),
                triggers: Some(m.triggers_json.to_string()),
                ..Default::default()
            })
            .collect(),
        settings: Default::default(),
        builtin: true,
    }
}

fn custom_to_view(t: CoworkTemplate) -> TemplateView {
    TemplateView {
        id: t.id,
        name: t.name,
        description: t.description,
        icon: t.icon,
        manager: t.manager_folder,
        manager_role: t.manager_role,
        members: t.members,
        settings: t.settings,
        builtin: false,
    }
}

/// GET /api/cowork/templates
///
/// Returns built-in blueprints followed by user-authored custom templates.
/// Both can be instantiated; only custom ones can be edited/deleted.
pub(crate) async fn list_templates(
    State(s): State<Arc<UiState>>,
) -> Result<Json<Vec<TemplateView>>, AppError> {
    let mut out: Vec<TemplateView> = BUILTIN_TEMPLATES.iter().map(builtin_to_view).collect();
    if let Some(db) = s.db.as_ref() {
        if let Ok(customs) = db.list_cowork_templates() {
            out.extend(customs.into_iter().map(custom_to_view));
        }
    }
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub(crate) struct TemplateBody {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    pub manager_folder: String,
    #[serde(default)]
    pub manager_role: Option<String>,
    #[serde(default)]
    pub members: Vec<TeamMember>,
    #[serde(default)]
    pub settings: Option<CoworkTeamSettings>,
}

/// POST /api/cowork/templates — create a custom template.
pub(crate) async fn create_template(
    State(s): State<Arc<UiState>>,
    Json(body): Json<TemplateBody>,
) -> Result<Json<TemplateView>, AppError> {
    let db = db(&s)?;
    if body.name.trim().is_empty() || body.manager_folder.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "name and manager_folder are required".into(),
        ));
    }
    let now = local_iso_string_now();
    let tmpl = CoworkTemplate {
        id: Uuid::new_v4().to_string(),
        name: body.name.trim().to_string(),
        description: body.description.unwrap_or_default(),
        icon: body
            .icon
            .filter(|i| !i.trim().is_empty())
            .unwrap_or_else(|| "🧩".into()),
        manager_folder: body.manager_folder.trim().to_string(),
        manager_role: body
            .manager_role
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| "lead".into()),
        members: body.members,
        settings: body.settings.unwrap_or_default(),
        created_at: now.clone(),
        updated_at: now,
    };
    db.insert_cowork_template(&tmpl)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(custom_to_view(tmpl)))
}

/// PUT /api/cowork/templates/:id — update a custom template. Built-in ids 404.
pub(crate) async fn update_template(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<TemplateBody>,
) -> Result<Json<TemplateView>, AppError> {
    let db = db(&s)?;
    let existing = db
        .get_cowork_template(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("template not found: {id}")))?;
    let tmpl = CoworkTemplate {
        id,
        name: body.name.trim().to_string(),
        description: body.description.unwrap_or_default(),
        icon: body
            .icon
            .filter(|i| !i.trim().is_empty())
            .unwrap_or(existing.icon),
        manager_folder: body.manager_folder.trim().to_string(),
        manager_role: body
            .manager_role
            .filter(|r| !r.trim().is_empty())
            .unwrap_or(existing.manager_role),
        members: body.members,
        settings: body.settings.unwrap_or_default(),
        created_at: existing.created_at,
        updated_at: local_iso_string_now(),
    };
    db.update_cowork_template(&tmpl)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(custom_to_view(tmpl)))
}

/// DELETE /api/cowork/templates/:id — delete a custom template.
pub(crate) async fn delete_template(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    db.delete_cowork_template(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct SaveAsTemplateBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
}

/// POST /api/cowork/teams/:id/save-as-template — snapshot an existing team
/// (manager + members + settings) into a reusable custom template.
pub(crate) async fn save_team_as_template(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<SaveAsTemplateBody>,
) -> Result<Json<TemplateView>, AppError> {
    let db = db(&s)?;
    let team = db
        .get_cowork_team(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("team not found: {id}")))?;
    let now = local_iso_string_now();
    let tmpl = CoworkTemplate {
        id: Uuid::new_v4().to_string(),
        name: body
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| format!("{} (template)", team.name)),
        description: body.description.unwrap_or_default(),
        icon: body
            .icon
            .filter(|i| !i.trim().is_empty())
            .unwrap_or_else(|| "🧩".into()),
        manager_folder: team.manager_folder,
        manager_role: "lead".into(),
        members: team.members,
        settings: team.settings,
        created_at: now.clone(),
        updated_at: now,
    };
    db.insert_cowork_template(&tmpl)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(custom_to_view(tmpl)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateTeamBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub manager_folder: Option<String>,
    #[serde(default)]
    pub workspace_dir: Option<String>,
    #[serde(default)]
    pub settings: Option<CoworkTeamSettings>,
}

/// PATCH /api/cowork/teams/:id — update team name / manager / workspace /
/// behaviour settings. Members are managed via the members endpoints.
pub(crate) async fn update_team(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<UpdateTeamBody>,
) -> Result<Json<TeamView>, AppError> {
    let db = db(&s)?;
    let team = db
        .get_cowork_team(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("team not found: {id}")))?;

    let name = body
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or(team.name);
    let manager_folder = body
        .manager_folder
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or(team.manager_folder);
    let workspace_dir = match body.workspace_dir {
        // explicit empty string clears it; absent keeps existing
        Some(w) if w.trim().is_empty() => None,
        Some(w) => Some(w.trim().to_string()),
        None => team.workspace_dir,
    };
    let settings = body.settings.unwrap_or(team.settings);

    db.update_cowork_team(
        &id,
        &name,
        &manager_folder,
        workspace_dir.as_deref(),
        &settings,
    )
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;

    // Keep the materialised chat group's name/folder in sync.
    if let Some(gm) = s.group_manager.as_ref() {
        let jid = format!("cowork:{id}");
        let binding = GroupBinding {
            jid,
            folder: manager_folder.clone(),
            name: name.clone(),
            channel: String::new(),
            group_type: "cowork".to_string(),
            requires_trigger: false,
            allowed_tools: None,
            allowed_paths: None,
            allowed_work_dirs: workspace_dir.as_ref().map(|w| vec![w.clone()]),
            bot_token: None,
            max_messages: None,
            llm_config_id: None,
            last_active: None,
            added_at: local_iso_string_now(),
        };
        gm.register(&db, &s.config, &binding);
    }

    let updated = db
        .get_cowork_team(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "team vanished".into()))?;
    Ok(Json(to_view(updated)))
}

/// GET /api/cowork/personas
///
/// Returns the names of personas available for picking as members. Sourced
/// from the live PersonaRegistry so both built-in and user-defined
/// personas surface in the picker.
#[derive(Debug, Serialize)]
pub(crate) struct PersonaView {
    pub name: String,
    pub description: String,
}

pub(crate) async fn list_personas(
    State(s): State<Arc<UiState>>,
) -> Result<Json<Vec<PersonaView>>, AppError> {
    let reg = s.persona_registry.as_ref().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "persona_registry not wired".into(),
        )
    })?;
    let guard = reg.lock().map_err(|_| {
        AppError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "persona_registry poisoned".into(),
        )
    })?;
    let personas = guard
        .list()
        .into_iter()
        .map(|p| PersonaView {
            name: p.name.clone(),
            description: p.description.clone(),
        })
        .collect();
    Ok(Json(personas))
}

#[derive(Debug, Deserialize)]
pub(crate) struct InstantiateTemplateBody {
    pub template_id: String,
    /// Optional user-provided name override.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub workspace_dir: Option<String>,
}

/// POST /api/cowork/teams/from-template
///
/// Spin up a team from a built-in template. The template's persona names
/// are mapped to agent folders — if an agent for a persona doesn't exist
/// yet, this auto-creates one with the persona's SOUL.md as core_prompt.
pub(crate) async fn create_from_template(
    State(s): State<Arc<UiState>>,
    Json(body): Json<InstantiateTemplateBody>,
) -> Result<Json<TeamView>, AppError> {
    let db = db(&s)?;

    // Resolve the template: prefer a user-authored custom template (DB), then
    // fall back to a built-in blueprint. Both collapse to the same owned shape.
    let resolved: ResolvedTemplate = if let Some(custom) = db
        .get_cowork_template(&body.template_id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
    {
        ResolvedTemplate {
            name: custom.name,
            manager: custom.manager_folder,
            members: custom.members,
            settings: custom.settings,
        }
    } else if let Some(tmpl) = BUILTIN_TEMPLATES.iter().find(|t| t.id == body.template_id) {
        ResolvedTemplate {
            name: tmpl.name.to_string(),
            manager: tmpl.manager.to_string(),
            members: tmpl
                .members
                .iter()
                .map(|m| TeamMember {
                    folder: m.folder.to_string(),
                    role: Some(m.role.to_string()),
                    responsibilities: Some(m.responsibilities.to_string()),
                    triggers: Some(m.triggers_json.to_string()),
                    ..Default::default()
                })
                .collect(),
            settings: Default::default(),
        }
    } else {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("template not found: {}", body.template_id),
        ));
    };
    let tmpl = &resolved;

    // Ensure each persona has a corresponding agent profile (folder).
    // The persona name itself becomes the folder slug.
    let ensure_agent = |slug: &str| -> Result<(), AppError> {
        if db
            .get_agent_by_folder(slug)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
            .is_some()
        {
            return Ok(());
        }
        // Look up persona content via the registry to seed SOUL.md.
        let core_prompt = s
            .persona_registry
            .as_ref()
            .and_then(|r| {
                r.lock()
                    .ok()
                    .and_then(|g| g.get(slug).map(|p| p.system_prompt.clone()))
            })
            .unwrap_or_default();
        let now = local_iso_string_now();
        // Use the agent_manager via UiState's existing path if available;
        // otherwise insert directly.
        db.insert_agent(slug, slug, false, None, None, &core_prompt, None, &now)
            .map_err(|e| {
                AppError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("agent create {slug}: {e}"),
                )
            })?;
        // Write SOUL.md + scaffold dirs.
        crate::gateway::group_manager::ensure_agent_dirs(&s.config, slug, slug);
        if !core_prompt.trim().is_empty() {
            crate::gateway::group_manager::write_soul_md(&s.config, slug, slug, &core_prompt);
        }
        Ok(())
    };
    ensure_agent(&tmpl.manager)?;
    // Seed persona files for each member into virtual-agents/ (no agent
    // row created — members are personas, not Profiles, per v0.7).
    let seed_persona = |slug: &str, role: &str, resp: &str| {
        let dest = s.config.paths.virtual_agents_dir.join(format!("{slug}.md"));
        if dest.exists() {
            return;
        }
        let _ = std::fs::create_dir_all(&s.config.paths.virtual_agents_dir);
        let body = format!(
            "---\nname: {slug}\ndescription: {role} — {resp}\nrole: {role}\n---\n\n# Responsibilities\n\n{resp}\n\n# How you work\n\n\
             You are a specialist member of a cowork team. Take the task the lead delegates to you, \
             do exactly that scope, and report results concisely. Do not branch into other roles.\n"
        );
        let _ = std::fs::write(dest, body);
    };
    for m in tmpl.members.iter() {
        seed_persona(
            &m.folder,
            m.role.as_deref().unwrap_or("member"),
            m.responsibilities.as_deref().unwrap_or(""),
        );
    }

    // Reuse create_team via direct CoworkTeam construction. Members carry
    // their template-supplied role / responsibilities / triggers so the
    // freshly minted team is dispatch-ready without manual edits.
    let id = Uuid::new_v4().to_string();
    let now = local_iso_string_now();
    let team = CoworkTeam {
        id: id.clone(),
        name: body
            .name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| tmpl.name.clone()),
        manager_folder: tmpl.manager.clone(),
        members: tmpl.members.clone(),
        workspace_dir: body
            .workspace_dir
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        created_at: now.clone(),
        settings: tmpl.settings.clone(),
    };
    db.insert_cowork_team(&team)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    let jid = format!("cowork:{id}");
    let binding = GroupBinding {
        jid: jid.clone(),
        folder: team.manager_folder.clone(),
        name: team.name.clone(),
        channel: String::new(),
        group_type: "cowork".to_string(),
        requires_trigger: false,
        allowed_tools: None,
        allowed_paths: None,
        allowed_work_dirs: team.workspace_dir.as_ref().map(|w| vec![w.clone()]),
        bot_token: None,
        max_messages: None,
        llm_config_id: None,
        last_active: None,
        added_at: now,
    };
    if let Some(gm) = s.group_manager.as_ref() {
        gm.register(&db, &s.config, &binding);
    }
    Ok(Json(to_view(team)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateMemberBody {
    /// Folder slug of the member to upsert. Required.
    pub folder: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub responsibilities: Option<String>,
    /// Trigger config — free-form text (cron expression, keyword list,
    /// JSON event matcher) the manager uses to decide when to delegate.
    #[serde(default)]
    pub triggers: Option<String>,
    #[serde(default)]
    pub handoff_rules: Option<String>,
    #[serde(default)]
    pub acceptance_criteria: Option<String>,
    #[serde(default)]
    pub output_format: Option<String>,
    #[serde(default)]
    pub sla: Option<String>,
    #[serde(default)]
    pub limits: Option<String>,
}

/// PUT /api/cowork/teams/:id/members
///
/// Upsert a single member's rich metadata (trigger, role, responsibilities,
/// handoff rules, etc.). If a member with this folder is not in the team
/// yet, it's appended; otherwise its fields are replaced. Returns the
/// updated team view.
pub(crate) async fn update_team_member(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<UpdateMemberBody>,
) -> Result<Json<TeamView>, AppError> {
    if body.folder.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "folder is required".into(),
        ));
    }
    let db = db(&s)?;
    let mut team = db
        .get_cowork_team(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("team not found: {id}")))?;

    let folder = body.folder.trim().to_string();
    let new_member = TeamMember {
        folder: folder.clone(),
        role: body.role.filter(|s| !s.trim().is_empty()),
        responsibilities: body.responsibilities.filter(|s| !s.trim().is_empty()),
        triggers: body.triggers.filter(|s| !s.trim().is_empty()),
        handoff_rules: body.handoff_rules.filter(|s| !s.trim().is_empty()),
        acceptance_criteria: body.acceptance_criteria.filter(|s| !s.trim().is_empty()),
        output_format: body.output_format.filter(|s| !s.trim().is_empty()),
        sla: body.sla.filter(|s| !s.trim().is_empty()),
        limits: body.limits.filter(|s| !s.trim().is_empty()),
    };

    if let Some(existing) = team.members.iter_mut().find(|m| m.folder == folder) {
        *existing = new_member;
    } else {
        team.members.push(new_member);
    }

    db.update_cowork_team_members(&id, &team.members)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;

    Ok(Json(to_view(team)))
}

/// DELETE /api/cowork/teams/:id/members/:folder
///
/// Remove a member from the team. The agent profile (and its SOUL.md /
/// MEMORY.md / chat history) is preserved — only the team membership row
/// goes away. Returns the updated team view.
pub(crate) async fn remove_team_member(
    State(s): State<Arc<UiState>>,
    AxumPath((id, folder)): AxumPath<(String, String)>,
) -> Result<Json<TeamView>, AppError> {
    let db = db(&s)?;
    let mut team = db
        .get_cowork_team(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("team not found: {id}")))?;

    team.members.retain(|m| m.folder != folder);
    db.update_cowork_team_members(&id, &team.members)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(to_view(team)))
}

/// DELETE /api/cowork/teams/:id
///
/// Removes the team row AND its materialised chat group so the sidebar
/// stops showing it. Messages persisted to the group's history stay in
/// the DB (deliberate — same as deleting any regular chat).
pub(crate) async fn delete_team(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let jid = format!("cowork:{id}");
    db.delete_cowork_team(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    // Cascade: tasks owned by this team go away with it.
    let _ = db.delete_cowork_team_tasks_for_team(&id);
    if let Some(gm) = s.group_manager.as_ref() {
        gm.unregister(&db, &s.config, &jid);
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ─── Cowork-only persona files ───────────────────────────────────────────────
//
// Cowork team MEMBERS are independent of user-facing Profiles. Each member
// is just a persona file living under `virtual_agents_dir/<name>.md`. These
// endpoints let the cowork UI read/write those files without touching the
// `agents` DB table (which is reserved for user Profiles).
//
// The same flat namespace is shared by all teams — `name` is a slug like
// `browser-agent` or `mvp-reviewer`. New names auto-create a fresh persona
// file (so the cowork "add member" flow can spawn brand-new specialists).

#[derive(Debug, Serialize)]
pub(crate) struct PersonaFileView {
    pub name: String,
    pub content: String,
    pub exists: bool,
}

fn persona_file_path(s: &UiState, name: &str) -> Result<std::path::PathBuf, AppError> {
    let slug = name.trim();
    if slug.is_empty() || slug.contains('/') || slug.contains("..") || slug.contains('\\') {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "invalid persona name".into(),
        ));
    }
    Ok(s.config.paths.virtual_agents_dir.join(format!("{slug}.md")))
}

/// GET /api/cowork/personas/:name/file
pub(crate) async fn get_persona_file(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<PersonaFileView>, AppError> {
    let path = persona_file_path(&s, &name)?;
    let (content, exists) = match std::fs::read_to_string(&path) {
        Ok(c) => (c, true),
        Err(_) => (String::new(), false),
    };
    Ok(Json(PersonaFileView {
        name,
        content,
        exists,
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PutPersonaFileBody {
    pub content: String,
}

/// PUT /api/cowork/personas/:name/file
///
/// Writes the persona markdown to disk. Creates the file if it didn't exist.
/// PersonaRegistry's hot-reload watcher will pick the change up within ~1.5s.
pub(crate) async fn put_persona_file(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<PutPersonaFileBody>,
) -> Result<Json<PersonaFileView>, AppError> {
    let path = persona_file_path(&s, &name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("mkdir: {e}")))?;
    }
    std::fs::write(&path, &body.content)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}")))?;
    Ok(Json(PersonaFileView {
        name,
        content: body.content,
        exists: true,
    }))
}

// ─── Tasks ──────────────────────────────────────────────────────────────────

/// GET /api/cowork/teams/:id/tasks
pub(crate) async fn list_team_tasks(
    State(s): State<Arc<UiState>>,
    AxumPath(team_id): AxumPath<String>,
) -> Result<Json<Vec<CoworkTeamTask>>, AppError> {
    let db = db(&s)?;
    let tasks = db
        .list_cowork_team_tasks(&team_id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(tasks))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateTaskBody {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub reviewer: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    #[serde(default)]
    pub due_at: Option<String>,
}

/// POST /api/cowork/teams/:id/tasks
pub(crate) async fn create_team_task(
    State(s): State<Arc<UiState>>,
    AxumPath(team_id): AxumPath<String>,
    Json(body): Json<CreateTaskBody>,
) -> Result<Json<CoworkTeamTask>, AppError> {
    if body.title.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "title is required".into(),
        ));
    }
    let db = db(&s)?;
    // Verify team exists.
    db.get_cowork_team(&team_id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("team not found: {team_id}")))?;

    let now = local_iso_string_now();
    let task = CoworkTeamTask {
        id: Uuid::new_v4().to_string(),
        team_id: team_id.clone(),
        title: body.title.trim().to_string(),
        description: body.description.filter(|s| !s.trim().is_empty()),
        status: body
            .status
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "todo".into()),
        assignee: body.assignee.filter(|s| !s.trim().is_empty()),
        reviewer: body.reviewer.filter(|s| !s.trim().is_empty()),
        priority: body
            .priority
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "medium".into()),
        depends_on: body.depends_on.unwrap_or_default(),
        result_output: None,
        created_at: now.clone(),
        updated_at: now,
        due_at: body.due_at.filter(|s| !s.trim().is_empty()),
        completed_at: None,
    };
    db.insert_cowork_team_task(&task)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(task))
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateTaskBody {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub assignee: Option<String>,
    pub reviewer: Option<String>,
    pub priority: Option<String>,
    pub depends_on: Option<Vec<String>>,
    pub result_output: Option<String>,
    pub due_at: Option<String>,
    pub completed_at: Option<String>,
}

/// PATCH /api/cowork/teams/:team_id/tasks/:task_id
pub(crate) async fn update_team_task(
    State(s): State<Arc<UiState>>,
    AxumPath((_team_id, task_id)): AxumPath<(String, String)>,
    Json(body): Json<UpdateTaskBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let now = local_iso_string_now();
    db.update_cowork_team_task(
        &task_id,
        body.title.as_deref(),
        body.description.as_deref(),
        body.status.as_deref(),
        body.assignee.as_deref(),
        body.reviewer.as_deref(),
        body.priority.as_deref(),
        body.depends_on.as_deref(),
        body.result_output.as_deref(),
        body.due_at.as_deref(),
        body.completed_at.as_deref(),
        &now,
    )
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/cowork/teams/:team_id/tasks/:task_id
pub(crate) async fn delete_team_task(
    State(s): State<Arc<UiState>>,
    AxumPath((_team_id, task_id)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    db.delete_cowork_team_task(&task_id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// =====================================================================
// GET /api/cowork/teams/:id/workspace?path=  — browse the team's workspace
// =====================================================================

#[derive(Debug, Deserialize)]
pub(crate) struct TeamBrowseQuery {
    /// Relative sub-path under the team's workspace_dir. Empty = workspace root.
    #[serde(default)]
    pub path: String,
}

/// List files under a cowork team's `workspace_dir` (one level). The `path`
/// query is a sanitized relative sub-path — `..` components are rejected so the
/// browse can never escape the workspace root.
pub(crate) async fn browse_team_workspace(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<TeamBrowseQuery>,
) -> Result<Json<super::workspace::WorkspaceListing>, AppError> {
    let team = db(&s)?
        .get_cowork_team(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "team not found".into()))?;
    let ws = team
        .workspace_dir
        .filter(|w| !w.is_empty())
        .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "team has no workspace".into()))?;
    let root = crate::util::paths::expand_tilde(&ws);

    let rel = std::path::Path::new(q.path.trim_start_matches('/'));
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(AppError(StatusCode::BAD_REQUEST, "invalid path".into()));
    }
    let target = if q.path.is_empty() {
        root
    } else {
        root.join(rel)
    };
    if !target.exists() || !target.is_dir() {
        return Err(AppError(StatusCode::NOT_FOUND, "path not found".into()));
    }

    let mut entries = Vec::new();
    super::workspace::walk(&target, 1, &mut entries);
    Ok(Json(super::workspace::WorkspaceListing {
        root: target.to_string_lossy().to_string(),
        entries,
    }))
}
