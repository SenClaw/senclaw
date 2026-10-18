//! `/api/projects` — the registry of projects, their context, and the sessions
//! created in them.
//!
//! The handlers hold a database handle and the config and nothing else, so the
//! routes can be built — and tested through a real router — without a running
//! daemon: a mistyped path parameter or query name compiles, then silently
//! never matches.
//!
//! `DELETE` archives unless `?purge=true`: archiving hides a project and stops
//! it taking new sessions, purging removes it and detaches its sessions. Neither
//! ever touches a file inside the project's root.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::db::Db;
use crate::projects::{
    self, check_root, ArchivedFilter, ContextWriteError, FactsWrite, NewProject, Project,
    ProjectPatch, ProjectStoreError, RootRules,
};

/// What the handlers need from the daemon.
#[derive(Clone)]
pub(crate) struct ProjectsCtx {
    pub(crate) db: Option<Arc<Db>>,
    pub(crate) config: Arc<Config>,
}

impl ProjectsCtx {
    fn db(&self) -> Result<&Arc<Db>, ApiError> {
        self.db
            .as_ref()
            .ok_or_else(|| ApiError::plain(StatusCode::SERVICE_UNAVAILABLE, "db_unset"))
    }

    fn rules(&self) -> RootRules {
        RootRules::from_config(&self.config)
    }
}

/// The routes, for any router state (the handlers use none of it).
pub(crate) fn routes<S: Clone + Send + Sync + 'static>(ctx: ProjectsCtx) -> Router<S> {
    Router::new()
        .route("/api/projects", get(projects_list).post(projects_create))
        // A literal sibling of `:id`; ids are `prj_` + hex, never this word.
        .route("/api/projects/validate-root", post(projects_validate_root))
        .route(
            "/api/projects/:id",
            get(projects_get)
                .patch(projects_update)
                .delete(projects_delete),
        )
        .route(
            "/api/projects/:id/context",
            get(projects_context_get).put(projects_context_put),
        )
        .route("/api/projects/:id/chats", get(projects_chats))
        .with_state(ctx)
}

// ===== Errors =====

/// `{"error": <code>, …}` — the code is for a client to branch on; extra fields
/// carry what it needs to explain (the reasons a root was refused, the project
/// that already has it, the context as it is now).
pub(crate) struct ApiError(StatusCode, Value);

impl ApiError {
    fn plain(status: StatusCode, code: &str) -> Self {
        Self(status, json!({ "error": code }))
    }

    fn with(status: StatusCode, code: &str, extra: Value) -> Self {
        let mut body = json!({ "error": code });
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        Self(status, body)
    }

    fn internal(e: impl std::fmt::Display) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": e.to_string() }),
        )
    }

    fn not_found() -> Self {
        Self::plain(StatusCode::NOT_FOUND, "project_not_found")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<ProjectStoreError> for ApiError {
    fn from(e: ProjectStoreError) -> Self {
        match e {
            ProjectStoreError::RootInUse { id, name } => Self::with(
                StatusCode::CONFLICT,
                "root_in_use",
                json!({ "projectId": id, "projectName": name }),
            ),
            ProjectStoreError::NotFound => Self::not_found(),
            ProjectStoreError::Invalid(why) => Self::with(
                StatusCode::BAD_REQUEST,
                "invalid_project",
                json!({ "message": why }),
            ),
            ProjectStoreError::Db(e) => Self::internal(e),
        }
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ===== Wire =====

/// A project as a client sees it: the row, plus what is only known by looking.
fn wire(p: &Project, chats: u32) -> Value {
    let root = std::path::Path::new(&p.root);
    let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("chatCount".into(), json!(chats));
        // The folder can be renamed or deleted after the project was saved; a
        // client shows that instead of waiting for a session to fail.
        o.insert("rootExists".into(), json!(root.is_dir()));
        o.insert(
            "hasClaudeMd".into(),
            json!(root.join("CLAUDE.md").is_file() || root.join("SENCLAW.md").is_file()),
        );
        o.insert("isGit".into(), json!(root.join(".git").exists()));
    }
    v
}

fn chats_of(db: &Db, id: &str) -> u32 {
    db.count_groups_per_project()
        .ok()
        .and_then(|m| m.get(id).copied())
        .unwrap_or(0)
}

fn load(ctx: &ProjectsCtx, id: &str) -> Result<Project, ApiError> {
    ctx.db()?
        .get_project(id)
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::not_found)
}

// ===== Roots =====

/// Check `raw` and say no with every reason, or return the canonical path.
fn accept_root(ctx: &ProjectsCtx, raw: &str) -> Result<String, ApiError> {
    let check = check_root(raw, &ctx.rules());
    match (check.ok(), check.canonical) {
        (true, Some(path)) => Ok(path.to_string_lossy().to_string()),
        _ => Err(ApiError::with(
            StatusCode::BAD_REQUEST,
            "invalid_root",
            json!({ "reasons": check.problems }),
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidateRootBody {
    path: String,
    /// When editing a project's root, the project itself — its own current
    /// root is not a conflict.
    #[serde(default, rename = "projectId")]
    project_id: Option<String>,
}

/// POST /api/projects/validate-root {path, projectId?} — the same checks a
/// create runs, without creating anything.
async fn projects_validate_root(
    State(ctx): State<ProjectsCtx>,
    Json(body): Json<ValidateRootBody>,
) -> ApiResult {
    let db = ctx.db()?;
    let check = check_root(&body.path, &ctx.rules());
    let mut reasons: Vec<Value> = check
        .problems
        .iter()
        .map(|p| json!({ "code": p.code, "message": p.message }))
        .collect();
    let mut in_use_by = Value::Null;
    if let Some(canonical) = &check.canonical {
        if let Some(owner) = db
            .find_project_by_root(&canonical.to_string_lossy())
            .map_err(ApiError::internal)?
        {
            if body.project_id.as_deref() != Some(owner.id.as_str()) {
                reasons.push(json!({
                    "code": "root_in_use",
                    "message": format!("{} is already the root of \"{}\".", canonical.display(), owner.name),
                }));
                in_use_by = json!({ "id": owner.id, "name": owner.name });
            }
        }
    }
    Ok(Json(json!({
        "ok": reasons.is_empty() && check.canonical.is_some(),
        "canonical": check.canonical.as_ref().map(|p| p.to_string_lossy().to_string()),
        "reasons": reasons,
        "hasClaudeMd": check.has_claude_md,
        "isGit": check.is_git,
        "inUseBy": in_use_by,
    })))
}

// ===== CRUD =====

#[derive(Deserialize)]
struct ListQuery {
    /// `false` (default): active projects; `true`: archived ones; `all`: both.
    #[serde(default)]
    archived: Option<String>,
}

/// GET /api/projects?archived=
async fn projects_list(State(ctx): State<ProjectsCtx>, Query(q): Query<ListQuery>) -> ApiResult {
    let filter = match q.archived.as_deref() {
        None | Some("") | Some("false") => ArchivedFilter::Active,
        Some("true") => ArchivedFilter::Archived,
        Some("all") => ArchivedFilter::All,
        Some(_) => {
            return Err(ApiError::plain(
                StatusCode::BAD_REQUEST,
                "invalid_archived_filter",
            ))
        }
    };
    let db = ctx.db()?;
    let rows = db.list_projects(filter).map_err(ApiError::internal)?;
    let counts = db.count_groups_per_project().unwrap_or_default();
    Ok(Json(json!({
        "projects": rows
            .iter()
            .map(|p| wire(p, counts.get(&p.id).copied().unwrap_or(0)))
            .collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: String,
    root: String,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "factsWrite")]
    facts_write: Option<String>,
    #[serde(default, rename = "inheritCommon")]
    inherit_common: Option<bool>,
}

fn parse_facts_write(raw: &str) -> Result<FactsWrite, ApiError> {
    FactsWrite::parse(raw).ok_or_else(|| {
        ApiError::with(
            StatusCode::BAD_REQUEST,
            "invalid_project",
            json!({ "message": "factsWrite must be project, chat or off" }),
        )
    })
}

/// POST /api/projects {name, root, description?, factsWrite?, inheritCommon?}
async fn projects_create(
    State(ctx): State<ProjectsCtx>,
    Json(body): Json<CreateBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let db = ctx.db()?;
    let facts_write = match body.facts_write.as_deref() {
        Some(w) => parse_facts_write(w)?,
        None => FactsWrite::Project,
    };
    let root = accept_root(&ctx, &body.root)?;
    let project = db.create_project(
        NewProject {
            name: body.name,
            root,
            description: body.description,
            facts_write,
            inherit_common: body.inherit_common.unwrap_or(true),
        },
        now_ms(),
    )?;
    Ok((StatusCode::CREATED, Json(wire(&project, 0))))
}

/// GET /api/projects/:id
async fn projects_get(State(ctx): State<ProjectsCtx>, Path(id): Path<String>) -> ApiResult {
    let p = load(&ctx, &id)?;
    let chats = chats_of(ctx.db()?, &p.id);
    Ok(Json(wire(&p, chats)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "factsWrite")]
    facts_write: Option<String>,
    #[serde(default, rename = "inheritCommon")]
    inherit_common: Option<bool>,
    #[serde(default)]
    archived: Option<bool>,
}

/// PATCH /api/projects/:id — any of name, root, description, factsWrite,
/// inheritCommon, archived.
///
/// A new root applies to sessions created afterwards, and to existing ones the
/// next time their core is built; a session already running keeps the folder
/// it started in.
async fn projects_update(
    State(ctx): State<ProjectsCtx>,
    Path(id): Path<String>,
    Json(body): Json<PatchBody>,
) -> ApiResult {
    let current = load(&ctx, &id)?;
    let facts_write = body
        .facts_write
        .as_deref()
        .map(parse_facts_write)
        .transpose()?;
    // A root that is not being changed is not judged again: a project whose
    // folder went missing must still be renameable.
    let root = match body.root.as_deref() {
        Some(raw) if raw.trim() != current.root => Some(accept_root(&ctx, raw)?),
        _ => None,
    };
    let updated = ctx.db()?.update_project(
        &id,
        ProjectPatch {
            name: body.name,
            root,
            description: body.description,
            facts_write,
            inherit_common: body.inherit_common,
            archived: body.archived,
        },
        now_ms(),
    )?;
    let chats = chats_of(ctx.db()?, &updated.id);
    Ok(Json(wire(&updated, chats)))
}

#[derive(Deserialize)]
struct DeleteQuery {
    #[serde(default)]
    purge: Option<bool>,
}

/// DELETE /api/projects/:id[?purge=true] — archive, or remove for good.
async fn projects_delete(
    State(ctx): State<ProjectsCtx>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult {
    let db = ctx.db()?;
    if q.purge != Some(true) {
        let archived = db.update_project(
            &id,
            ProjectPatch {
                archived: Some(true),
                ..Default::default()
            },
            now_ms(),
        )?;
        let chats = chats_of(db, &archived.id);
        return Ok(Json(wire(&archived, chats)));
    }
    let detached = db.delete_project(&id)?;
    let home = crate::util::paths::senclaw_home();
    for chat in &detached {
        // Their own workspace-state file was only meaningful inside the project.
        projects::remove_session_state_file(&home, &chat.folder, &chat.jid);
    }
    if let Err(e) = projects::context::remove_project_files(&ctx.config.paths.projects_dir, &id) {
        tracing::warn!("[projects] could not remove the files of {id}: {e}");
    }
    Ok(Json(json!({
        "deleted": id,
        "detachedChats": detached.iter().map(|c| c.jid.clone()).collect::<Vec<_>>(),
    })))
}

// ===== Context =====

fn context_wire(file: &projects::ContextFile) -> Value {
    json!({
        "content": file.content,
        "updatedAt": file.updated_at,
        "chars": file.content.trim().chars().count(),
        "maxChars": projects::CONTEXT_MAX_CHARS,
    })
}

/// GET /api/projects/:id/context — `{content, updatedAt}`; an absent file is
/// empty with `updatedAt: 0`.
async fn projects_context_get(State(ctx): State<ProjectsCtx>, Path(id): Path<String>) -> ApiResult {
    let p = load(&ctx, &id)?;
    let file = projects::read_context(&ctx.config.paths.projects_dir, &p.id)
        .map_err(ApiError::internal)?;
    Ok(Json(context_wire(&file)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextBody {
    content: String,
    /// The `updatedAt` of the copy the caller edited; `0`/absent for "no file
    /// yet".
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<i64>,
}

/// PUT /api/projects/:id/context {content, updatedAt} — refused with 409 (and
/// the current text) when the file changed after the caller read it.
async fn projects_context_put(
    State(ctx): State<ProjectsCtx>,
    Path(id): Path<String>,
    Json(body): Json<ContextBody>,
) -> ApiResult {
    let p = load(&ctx, &id)?;
    match projects::write_context(
        &ctx.config.paths.projects_dir,
        &p.id,
        &body.content,
        body.updated_at.unwrap_or(0),
    ) {
        Ok(file) => Ok(Json(context_wire(&file))),
        Err(ContextWriteError::Stale { current }) => Err(ApiError::with(
            StatusCode::CONFLICT,
            "context_changed",
            context_wire(&current),
        )),
        Err(ContextWriteError::TooLarge { max }) => Err(ApiError::with(
            StatusCode::PAYLOAD_TOO_LARGE,
            "context_too_large",
            json!({ "maxBytes": max }),
        )),
        Err(ContextWriteError::Io(e)) => Err(ApiError::internal(e)),
    }
}

// ===== Sessions =====

/// GET /api/projects/:id/chats — the sessions created in the project.
async fn projects_chats(State(ctx): State<ProjectsCtx>, Path(id): Path<String>) -> ApiResult {
    let p = load(&ctx, &id)?;
    let chats = ctx
        .db()?
        .list_groups_in_project(&p.id)
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "chats": chats
            .iter()
            .map(|g| json!({
                "jid": g.jid,
                "name": g.name,
                "folder": g.folder,
                "groupType": g.group_type,
                "lastActive": g.last_active,
                "addedAt": g.added_at,
            }))
            .collect::<Vec<_>>(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GroupBinding;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    struct Fixture {
        ctx: ProjectsCtx,
        dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::from_env();
        config.paths.projects_dir = dir.path().join("data").join("projects");
        let ctx = ProjectsCtx {
            db: Some(Arc::new(Db::open_in_memory(&config).unwrap())),
            config: Arc::new(config),
        };
        Fixture { ctx, dir }
    }

    impl Fixture {
        /// A real folder to use as a root.
        fn folder(&self, name: &str) -> String {
            let p = self.dir.path().join("repos").join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::canonicalize(&p)
                .unwrap()
                .to_string_lossy()
                .to_string()
        }

        async fn call(
            &self,
            method: Method,
            uri: &str,
            body: Option<Value>,
        ) -> (StatusCode, Value) {
            let app: Router<()> = routes(self.ctx.clone());
            let mut req = Request::builder().method(method).uri(uri);
            let body = match body {
                Some(b) => {
                    req = req.header("content-type", "application/json");
                    Body::from(b.to_string())
                }
                None => Body::empty(),
            };
            let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            )
        }

        async fn create(&self, name: &str, root: &str) -> Value {
            let (status, body) = self
                .call(
                    Method::POST,
                    "/api/projects",
                    Some(json!({ "name": name, "root": root })),
                )
                .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            body
        }
    }

    fn reason_codes(body: &Value) -> Vec<String> {
        body["reasons"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|r| r["code"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn creating_a_project_returns_the_row_with_what_only_looking_can_tell() {
        let f = fixture();
        let root = f.folder("sen");
        std::fs::create_dir_all(format!("{root}/.git")).unwrap();
        let p = f.create("Sen", &root).await;
        assert!(p["id"].as_str().unwrap().starts_with("prj_"));
        assert_eq!(
            (p["name"].as_str(), p["root"].as_str()),
            (Some("Sen"), Some(root.as_str()))
        );
        assert_eq!(
            (p["factsWrite"].as_str(), p["inheritCommon"].as_bool()),
            (Some("project"), Some(true))
        );
        assert_eq!(
            (
                p["rootExists"].as_bool(),
                p["isGit"].as_bool(),
                p["hasClaudeMd"].as_bool()
            ),
            (Some(true), Some(true), Some(false))
        );
        assert_eq!(p["chatCount"], 0);
        assert!(p["archivedAt"].is_null());
        for key in ["createdAt", "updatedAt", "description"] {
            assert!(p.get(key).is_some(), "missing {key}");
        }

        let (status, again) = f
            .call(
                Method::GET,
                &format!("/api/projects/{}", p["id"].as_str().unwrap()),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(again, p);
    }

    #[tokio::test]
    async fn an_unsafe_root_is_refused_with_every_reason_and_nothing_is_created() {
        let f = fixture();
        let home = dirs::home_dir().unwrap().to_string_lossy().to_string();
        for (root, code) in [
            ("/", "filesystem_root"),
            (home.as_str(), "home_dir"),
            ("relative/dir", "not_absolute"),
            ("", "empty"),
        ] {
            let (status, body) = f
                .call(
                    Method::POST,
                    "/api/projects",
                    Some(json!({ "name": "x", "root": root })),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{root}: {body}");
            assert_eq!(body["error"], "invalid_root");
            assert!(
                reason_codes(&body).contains(&code.to_string()),
                "{root}: {body}"
            );
            assert!(body["reasons"][0]["message"].as_str().unwrap().len() > 5);
        }
        let missing = format!("{}/nope", f.dir.path().display());
        let (_, body) = f
            .call(
                Method::POST,
                "/api/projects",
                Some(json!({ "name": "x", "root": missing })),
            )
            .await;
        assert_eq!(reason_codes(&body), ["not_found"]);
        let (_, list) = f
            .call(Method::GET, "/api/projects?archived=all", None)
            .await;
        assert_eq!(list["projects"].as_array().unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_same_folder_under_another_spelling_is_the_same_root() {
        let f = fixture();
        let root = f.folder("sen");
        let first = f.create("Sen", &root).await;
        // A trailing slash, a `..` detour and a symlink all resolve to it.
        let link = format!("{}/link", f.dir.path().display());
        std::os::unix::fs::symlink(&root, &link).unwrap();
        for spelling in [format!("{root}/"), format!("{root}/../sen"), link] {
            let (status, body) = f
                .call(
                    Method::POST,
                    "/api/projects",
                    Some(json!({ "name": "Dup", "root": spelling })),
                )
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{spelling}: {body}");
            assert_eq!(body["error"], "root_in_use");
            assert_eq!(body["projectId"], first["id"]);
            assert_eq!(body["projectName"], "Sen");
        }
        // Nested folders are different roots.
        let nested = f.folder("sen/sub");
        f.create("Nested", &nested).await;
    }

    #[tokio::test]
    async fn validate_root_answers_like_a_create_would_without_creating() {
        let f = fixture();
        let root = f.folder("sen");
        let post = |path: &str| json!({ "path": path });
        let (status, ok) = f
            .call(
                Method::POST,
                "/api/projects/validate-root",
                Some(post(&root)),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["canonical"], root.as_str());
        assert!(ok["reasons"].as_array().unwrap().is_empty());
        assert!(ok["inUseBy"].is_null());

        let (_, bad) = f
            .call(Method::POST, "/api/projects/validate-root", Some(post("/")))
            .await;
        assert_eq!(
            (bad["ok"].as_bool(), reason_codes(&bad)),
            (Some(false), vec!["filesystem_root".to_string()])
        );

        let created = f.create("Sen", &root).await;
        let (_, used) = f
            .call(
                Method::POST,
                "/api/projects/validate-root",
                Some(post(&root)),
            )
            .await;
        assert_eq!(used["ok"], false);
        assert_eq!(reason_codes(&used), ["root_in_use"]);
        assert_eq!(used["inUseBy"]["id"], created["id"]);
        // Editing the project that owns the root is not a conflict.
        let (_, own) = f
            .call(
                Method::POST,
                "/api/projects/validate-root",
                Some(json!({ "path": root, "projectId": created["id"] })),
            )
            .await;
        assert_eq!(own["ok"], true);
        let (_, list) = f.call(Method::GET, "/api/projects", None).await;
        assert_eq!(
            list["projects"].as_array().unwrap().len(),
            1,
            "validating created nothing"
        );
    }

    #[tokio::test]
    async fn a_patch_edits_fields_and_validates_a_new_root() {
        let f = fixture();
        let a = f.create("A", &f.folder("a")).await;
        let b = f.create("B", &f.folder("b")).await;
        let id = a["id"].as_str().unwrap();

        let (status, body) = f
            .call(
                Method::PATCH,
                &format!("/api/projects/{id}"),
                Some(json!({ "name": "Alpha", "description": "first", "factsWrite": "off", "inheritCommon": false })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            (
                body["name"].as_str(),
                body["description"].as_str(),
                body["factsWrite"].as_str(),
                body["inheritCommon"].as_bool()
            ),
            (Some("Alpha"), Some("first"), Some("off"), Some(false))
        );
        assert_eq!(body["root"], a["root"], "untouched");

        // Onto another project's root, onto an unsafe folder, with a bad enum.
        let (status, body) = f
            .call(
                Method::PATCH,
                &format!("/api/projects/{id}"),
                Some(json!({ "root": b["root"] })),
            )
            .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::CONFLICT, Some("root_in_use"))
        );
        let (status, body) = f
            .call(
                Method::PATCH,
                &format!("/api/projects/{id}"),
                Some(json!({ "root": "/" })),
            )
            .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_root"))
        );
        let (status, _) = f
            .call(
                Method::PATCH,
                &format!("/api/projects/{id}"),
                Some(json!({ "factsWrite": "everywhere" })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // A field nobody knows is a typo, not something to ignore.
        let (status, _) = f
            .call(
                Method::PATCH,
                &format!("/api/projects/{id}"),
                Some(json!({ "inheritcommon": false })),
            )
            .await;
        assert!(status.is_client_error());

        // A moved root is accepted, and the old folder is free for another project.
        let moved = f.folder("a2");
        let (status, body) = f
            .call(
                Method::PATCH,
                &format!("/api/projects/{id}"),
                Some(json!({ "root": moved })),
            )
            .await;
        assert_eq!(
            (status, body["root"].as_str()),
            (StatusCode::OK, Some(moved.as_str()))
        );
        f.create("Reuse", a["root"].as_str().unwrap()).await;
    }

    #[tokio::test]
    async fn a_project_whose_folder_vanished_can_still_be_renamed_and_says_so() {
        let f = fixture();
        let root = f.folder("gone");
        let p = f.create("Gone", &root).await;
        std::fs::remove_dir_all(&root).unwrap();
        let id = p["id"].as_str().unwrap();
        let (_, shown) = f
            .call(Method::GET, &format!("/api/projects/{id}"), None)
            .await;
        assert_eq!(shown["rootExists"], false);
        // Sending the stored root back unchanged is not "a new root".
        let (status, body) = f
            .call(
                Method::PATCH,
                &format!("/api/projects/{id}"),
                Some(json!({ "name": "Renamed", "root": root })),
            )
            .await;
        assert_eq!(
            (status, body["name"].as_str()),
            (StatusCode::OK, Some("Renamed"))
        );
    }

    #[tokio::test]
    async fn delete_archives_and_purge_removes_and_detaches_the_chats() {
        let f = fixture();
        let p = f.create("Sen", &f.folder("sen")).await;
        let id = p["id"].as_str().unwrap().to_string();
        let db = f.ctx.db.clone().unwrap();
        for jid in ["web:main:1", "web:main:2"] {
            db.upsert_group(&GroupBinding {
                jid: jid.into(),
                folder: "main".into(),
                name: jid.into(),
                channel: String::new(),
                group_type: "chat".into(),
                requires_trigger: false,
                allowed_tools: None,
                allowed_paths: None,
                allowed_work_dirs: None,
                bot_token: None,
                max_messages: None,
                llm_config_id: None,
                last_active: None,
                added_at: "2026-10-18T00:00:00Z".into(),
                project_id: Some(id.clone()),
            })
            .unwrap();
        }
        let (_, with_chats) = f
            .call(Method::GET, &format!("/api/projects/{id}"), None)
            .await;
        assert_eq!(with_chats["chatCount"], 2);
        let (_, chats) = f
            .call(Method::GET, &format!("/api/projects/{id}/chats"), None)
            .await;
        assert_eq!(chats["chats"].as_array().unwrap().len(), 2);
        assert_eq!(chats["chats"][0]["groupType"], "chat");

        // Plain DELETE only archives: hidden from the default list, sessions untouched.
        let (status, archived) = f
            .call(Method::DELETE, &format!("/api/projects/{id}"), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(archived["archivedAt"].is_i64());
        let (_, active) = f.call(Method::GET, "/api/projects", None).await;
        assert!(active["projects"].as_array().unwrap().is_empty());
        let (_, only_archived) = f
            .call(Method::GET, "/api/projects?archived=true", None)
            .await;
        assert_eq!(only_archived["projects"].as_array().unwrap().len(), 1);
        assert_eq!(
            db.get_group("web:main:1")
                .unwrap()
                .unwrap()
                .project_id
                .as_deref(),
            Some(id.as_str())
        );
        let (status, _) = f
            .call(Method::GET, "/api/projects?archived=sometimes", None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Give it a context file, then purge.
        let (_, put) = f
            .call(
                Method::PUT,
                &format!("/api/projects/{id}/context"),
                Some(json!({ "content": "hello", "updatedAt": 0 })),
            )
            .await;
        assert_eq!(put["content"], "hello");
        let context_dir = f.ctx.config.paths.projects_dir.join(&id);
        assert!(context_dir.join("CONTEXT.md").is_file());
        let root = p["root"].as_str().unwrap().to_string();
        std::fs::write(format!("{root}/keep.txt"), "mine").unwrap();

        let (status, gone) = f
            .call(
                Method::DELETE,
                &format!("/api/projects/{id}?purge=true"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{gone}");
        assert_eq!(gone["deleted"], id.as_str());
        assert_eq!(gone["detachedChats"].as_array().unwrap().len(), 2);
        assert!(!context_dir.exists());
        assert_eq!(
            std::fs::read_to_string(format!("{root}/keep.txt")).unwrap(),
            "mine",
            "the root is never touched"
        );
        for jid in ["web:main:1", "web:main:2"] {
            assert_eq!(db.get_group(jid).unwrap().unwrap().project_id, None);
        }
        let (status, _) = f
            .call(Method::GET, &format!("/api/projects/{id}"), None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = f
            .call(
                Method::DELETE,
                &format!("/api/projects/{id}?purge=true"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_context_round_trips_and_a_stale_put_gets_409_with_the_current_text() {
        let f = fixture();
        let p = f.create("Sen", &f.folder("sen")).await;
        let url = format!("/api/projects/{}/context", p["id"].as_str().unwrap());

        let (status, empty) = f.call(Method::GET, &url, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (
                empty["content"].as_str(),
                empty["updatedAt"].as_i64(),
                empty["maxChars"].as_u64()
            ),
            (Some(""), Some(0), Some(8000))
        );

        // `updatedAt` may be left out for a project with no file yet.
        let (status, first) = f
            .call(Method::PUT, &url, Some(json!({ "content": "v1" })))
            .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        let t1 = first["updatedAt"].as_i64().unwrap();
        assert!(t1 > 0);
        let (_, second) = f
            .call(
                Method::PUT,
                &url,
                Some(json!({ "content": "v2", "updatedAt": t1 })),
            )
            .await;
        assert!(second["updatedAt"].as_i64().unwrap() > t1);

        // The first writer, still holding t1.
        let (status, conflict) = f
            .call(
                Method::PUT,
                &url,
                Some(json!({ "content": "mine", "updatedAt": t1 })),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(conflict["error"], "context_changed");
        assert_eq!(conflict["content"], "v2");
        assert_eq!(conflict["updatedAt"], second["updatedAt"]);
        let (_, now) = f.call(Method::GET, &url, None).await;
        assert_eq!(now["content"], "v2", "a refused write changes nothing");

        let huge = "x".repeat(projects::context::CONTEXT_FILE_MAX_BYTES + 1);
        let (status, _) = f
            .call(
                Method::PUT,
                &url,
                Some(json!({ "content": huge, "updatedAt": second["updatedAt"] })),
            )
            .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn unknown_projects_are_404_and_a_missing_database_is_503() {
        let f = fixture();
        for (method, uri) in [
            (Method::GET, "/api/projects/prj_00000000"),
            (Method::GET, "/api/projects/prj_00000000/context"),
            (Method::GET, "/api/projects/prj_00000000/chats"),
        ] {
            let (status, body) = f.call(method, uri, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(body["error"], "project_not_found");
        }
        // An id is never a path: nothing outside the projects folder is read.
        let (status, _) = f
            .call(Method::GET, "/api/projects/..%2F..%2Fetc/context", None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let bare = Fixture {
            ctx: ProjectsCtx {
                db: None,
                config: f.ctx.config.clone(),
            },
            dir: tempfile::tempdir().unwrap(),
        };
        let (status, body) = bare.call(Method::GET, "/api/projects", None).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, Some("db_unset"))
        );
    }

    #[tokio::test]
    async fn the_list_is_sorted_by_name_and_counts_each_projects_chats() {
        let f = fixture();
        f.create("beta", &f.folder("b")).await;
        let a = f.create("Alpha", &f.folder("a")).await;
        f.ctx
            .db
            .clone()
            .unwrap()
            .upsert_group(&GroupBinding {
                jid: "web:main:9".into(),
                folder: "main".into(),
                name: "x".into(),
                channel: String::new(),
                group_type: "code".into(),
                requires_trigger: false,
                allowed_tools: None,
                allowed_paths: None,
                allowed_work_dirs: None,
                bot_token: None,
                max_messages: None,
                llm_config_id: None,
                last_active: None,
                added_at: "2026-10-18T00:00:00Z".into(),
                project_id: Some(a["id"].as_str().unwrap().into()),
            })
            .unwrap();
        let (_, list) = f.call(Method::GET, "/api/projects", None).await;
        let rows = list["projects"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|p| p["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["Alpha", "beta"]
        );
        assert_eq!(
            (rows[0]["chatCount"].as_u64(), rows[1]["chatCount"].as_u64()),
            (Some(1), Some(0))
        );
    }
}
