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
use axum::routing::{delete, get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::db::Db;
use crate::memory::cognitive::retrievers::build_fts_match;
use crate::memory::engine::facts::store::{FactKind, Inserted, NewFact, Scope};
use crate::memory::engine::facts::{self, defense, extract, FactEngine, FactStore};
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
        .with_state(ctx.clone())
        .merge(facts_routes(ctx))
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
    // Its facts go first, a slice at a time. Were the row to go first, a
    // failure half-way would leave a bank that no project owns and nothing
    // to retry it from; this way the project is still there to purge again.
    load(&ctx, &id)?;
    let deleted_facts = purge_fact_bank(&ctx, &id).await?;
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
        "deletedFacts": deleted_facts,
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

// ===== Facts =====

/// How a request finds the fact engine, which is where the embedding model
/// lives: the process-wide one in the daemon, a private one in a test. Listing,
/// forgetting and purging need only the database; adding a fact uses the model
/// when there is one.
type EngineResolver = Arc<dyn Fn() -> Option<Arc<FactEngine>> + Send + Sync>;

#[derive(Clone)]
struct FactsState {
    ctx: ProjectsCtx,
    engine: EngineResolver,
}

impl FactsState {
    fn store(&self) -> Result<FactStore, ApiError> {
        Ok(FactStore::new(Arc::clone(self.ctx.db()?)))
    }
}

/// The routes of a project's own facts, for any router state.
pub(crate) fn facts_routes<S: Clone + Send + Sync + 'static>(ctx: ProjectsCtx) -> Router<S> {
    facts_router(ctx, Arc::new(facts::instance))
}

fn facts_router<S: Clone + Send + Sync + 'static>(
    ctx: ProjectsCtx,
    engine: EngineResolver,
) -> Router<S> {
    Router::new()
        .route(
            "/api/projects/:id/facts",
            get(project_facts_list).post(project_facts_add),
        )
        .route(
            "/api/projects/:id/facts/:fact_id",
            delete(project_fact_forget),
        )
        .with_state(FactsState { ctx, engine })
}

const FACTS_DEFAULT_LIMIT: u32 = 50;
const FACTS_MAX_LIMIT: u32 = 200;

#[derive(Deserialize)]
struct FactsQuery {
    /// Keyword search; omitted → newest first.
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    /// Also list facts that were replaced (they carry `supersededBy`). Only the
    /// newest-first listing has a history.
    #[serde(default)]
    history: Option<bool>,
}

/// GET /api/projects/:id/facts?q=&limit=&history= — the project's own facts,
/// newest first or best keyword matches. The agent's common facts are not
/// here; they are listed under the agent's bank.
async fn project_facts_list(
    State(st): State<FactsState>,
    Path(id): Path<String>,
    Query(q): Query<FactsQuery>,
) -> ApiResult {
    let p = load(&st.ctx, &id)?;
    let bank = projects::project_bank(&p.id);
    let store = st.store()?;
    let limit = q
        .limit
        .unwrap_or(FACTS_DEFAULT_LIMIT)
        .clamp(1, FACTS_MAX_LIMIT) as usize;
    let found = match q.q.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => match build_fts_match(text) {
            Some(m) => store.search_keyword(&bank, &m, None, limit, Scope::Agent),
            None => Ok(Vec::new()),
        },
        None => store.recent(&bank, limit, q.history.unwrap_or(false), Scope::Agent),
    }
    .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "bank": bank,
        "total": store.count(&bank).map_err(ApiError::internal)?,
        "count": found.len(),
        "facts": found.iter().map(super::memory_facts::wire).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FactBody {
    text: String,
    /// `world` (default) or `experience`.
    #[serde(default)]
    kind: Option<String>,
    /// When it happened: `2026-03`, `2026-03-02`, or a range `a/b`. Omitted for
    /// a standing fact.
    #[serde(default)]
    when: Option<String>,
}

fn invalid_fact(code: &str, message: String) -> ApiError {
    ApiError::with(StatusCode::BAD_REQUEST, code, json!({ "message": message }))
}

/// POST /api/projects/:id/facts {text, kind?, when?} — add a fact by hand.
///
/// No model reads it: the text is stored as typed (whitespace collapsed), with
/// a vector when an embedding model is configured. It is marked `manual`, and
/// nothing a conversation says replaces it. The same words already in the
/// project become this fact. A secret in the text is refused, since a fact is
/// shown to the model whenever it is relevant.
async fn project_facts_add(
    State(st): State<FactsState>,
    Path(id): Path<String>,
    Json(body): Json<FactBody>,
) -> Result<Response, ApiError> {
    let p = load(&st.ctx, &id)?;
    let text = body.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let len = text.chars().count();
    if !(extract::MIN_FACT_CHARS..=extract::MAX_FACT_CHARS).contains(&len) {
        return Err(invalid_fact(
            "invalid_fact",
            format!(
                "a fact is {} to {} characters, this one is {len}",
                extract::MIN_FACT_CHARS,
                extract::MAX_FACT_CHARS
            ),
        ));
    }
    if let defense::Verdict::Blocked { kinds } = defense::scrub(&text, defense::Mode::Block) {
        return Err(ApiError::with(
            StatusCode::BAD_REQUEST,
            "secret_in_fact",
            json!({ "kinds": kinds }),
        ));
    }
    let kind = match body
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        None => FactKind::World,
        Some(k) => FactKind::parse(k).ok_or_else(|| {
            invalid_fact(
                "invalid_kind",
                format!("kind is world or experience, not {k:?}"),
            )
        })?,
    };
    let occurred = match body
        .when
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        None => None,
        Some(w) => Some(extract::parse_when(w).ok_or_else(|| {
            invalid_fact(
                "invalid_when",
                format!("when is a date like 2026-03 or 2026-03-02, or a range a/b, not {w:?}"),
            )
        })?),
    };

    let (embedding, model) = match (st.engine)() {
        Some(engine) => (
            engine
                .embed_all(std::slice::from_ref(&text))
                .await
                .pop()
                .flatten(),
            engine.model_key(),
        ),
        None => (None, None),
    };
    let bank = projects::project_bank(&p.id);
    let store = st.store()?;
    let inserted = store
        .insert_manual(
            &bank,
            &NewFact {
                text,
                kind,
                occurred,
                mentioned_at: chrono::Utc::now().timestamp(),
                entities: Vec::new(),
                // A vector made by a model other than the one that will search
                // is never compared, so one is stored only with its model.
                model: embedding.as_ref().and(model),
                embedding,
                source: facts::store::SOURCE_MANUAL.to_string(),
                origin: String::new(),
                slot: None,
            },
        )
        .map_err(ApiError::internal)?;
    let (fact_id, status, existing) = match inserted {
        Inserted::New(fid) => (fid, StatusCode::CREATED, false),
        Inserted::Existing(fid) => (fid, StatusCode::OK, true),
    };
    let fact = store
        .get(&fact_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal("the fact vanished after it was stored"))?;
    Ok((
        status,
        Json(json!({ "fact": super::memory_facts::wire(&fact), "existing": existing })),
    )
        .into_response())
}

/// DELETE /api/projects/:id/facts/:factId — forget one fact of the project. A
/// fact of another bank is a 404, whatever its id.
async fn project_fact_forget(
    State(st): State<FactsState>,
    Path((id, fact_id)): Path<(String, String)>,
) -> ApiResult {
    let p = load(&st.ctx, &id)?;
    let deleted = st
        .store()?
        .delete_in_bank(&projects::project_bank(&p.id), &fact_id)
        .map_err(ApiError::internal)?;
    if !deleted {
        return Err(ApiError::plain(StatusCode::NOT_FOUND, "fact_not_found"));
    }
    Ok(Json(json!({ "deleted": fact_id })))
}

/// Delete every fact of a project's bank, off the async runtime: a slice at a
/// time, with the database free in between (see `FactStore::delete_bank`).
/// Returns how many facts went.
async fn purge_fact_bank(ctx: &ProjectsCtx, id: &str) -> Result<usize, ApiError> {
    let store = FactStore::new(Arc::clone(ctx.db()?));
    let bank = projects::project_bank(id);
    tokio::task::spawn_blocking(move || store.delete_bank(&bank))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)
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

    // ===== Facts =====

    /// One dimension per known word, so similarity is shared vocabulary.
    struct Bag;

    #[async_trait::async_trait]
    impl crate::memory::embedding::EmbeddingProvider for Bag {
        fn name(&self) -> &str {
            "bag"
        }
        fn model(&self) -> &str {
            "v1"
        }
        fn dimensions(&self) -> u32 {
            3
        }
        async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let t = t.to_lowercase();
                    ["deploy", "friday", "staging"]
                        .iter()
                        .map(|w| t.matches(w).count() as f32)
                        .collect()
                })
                .collect())
        }
    }

    impl Fixture {
        fn store(&self) -> FactStore {
            FactStore::new(self.ctx.db.clone().unwrap())
        }

        /// The facts routes alone, with `engine` standing in for the daemon's.
        async fn facts_call(
            &self,
            engine: Option<Arc<FactEngine>>,
            method: Method,
            uri: &str,
            body: Option<Value>,
        ) -> (StatusCode, Value) {
            let resolver: EngineResolver = Arc::new(move || engine.clone());
            let app: Router<()> = facts_router(self.ctx.clone(), resolver);
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

        /// A fact said in a conversation, as retain would store it.
        fn said(&self, bank: &str, text: &str, origin: &str) -> String {
            match self
                .store()
                .insert(
                    bank,
                    &NewFact {
                        text: text.into(),
                        kind: FactKind::World,
                        occurred: None,
                        mentioned_at: 10,
                        entities: vec![],
                        embedding: None,
                        model: None,
                        source: "reflection".into(),
                        origin: origin.into(),
                        slot: None,
                    },
                )
                .unwrap()
            {
                Inserted::New(id) => id,
                other => panic!("{other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_fact_is_added_by_hand_listed_and_forgotten() {
        let f = fixture();
        let p = f.create("Sen", &f.folder("sen")).await;
        let id = p["id"].as_str().unwrap();
        let uri = format!("/api/projects/{id}/facts");

        // Through the merged router, the way the daemon serves it.
        let (status, empty) = f.call(Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::OK, "{empty}");
        assert_eq!(
            (empty["bank"].as_str(), empty["total"].as_u64()),
            (Some(format!("project:{id}").as_str()), Some(0))
        );

        let (status, added) = f
            .call(
                Method::POST,
                &uri,
                Some(json!({ "text": "  The   deploy is on Friday ", "when": "2026-10-23" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{added}");
        assert_eq!(added["existing"], false);
        let fact = &added["fact"];
        assert_eq!(fact["text"], "The deploy is on Friday");
        assert_eq!(fact["source"], "manual");
        assert_eq!(fact["origin"], "", "belongs to the project, not to a chat");
        assert_eq!(fact["kind"], "world");
        assert!(fact["occurredStart"].is_i64());
        let fact_id = fact["id"].as_str().unwrap().to_string();

        let (_, list) = f.call(Method::GET, &uri, None).await;
        assert_eq!(
            (list["total"].as_u64(), list["count"].as_u64()),
            (Some(1), Some(1))
        );
        let (_, found) = f.call(Method::GET, &format!("{uri}?q=friday"), None).await;
        assert_eq!(found["facts"][0]["id"], fact_id.as_str());
        let (_, none) = f.call(Method::GET, &format!("{uri}?q=tuesday"), None).await;
        assert_eq!(none["count"], 0);

        let (status, gone) = f
            .call(Method::DELETE, &format!("{uri}/{fact_id}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{gone}");
        assert_eq!(gone["deleted"], fact_id.as_str());
        let (status, again) = f
            .call(Method::DELETE, &format!("{uri}/{fact_id}"), None)
            .await;
        assert_eq!(
            (status, again["error"].as_str()),
            (StatusCode::NOT_FOUND, Some("fact_not_found"))
        );
        let (_, list) = f.call(Method::GET, &uri, None).await;
        assert_eq!(list["total"], 0);
    }

    #[tokio::test]
    async fn adding_a_fact_calls_no_model_and_the_same_words_become_the_typed_fact() {
        let f = fixture();
        let p = f.create("Sen", &f.folder("sen")).await;
        let id = p["id"].as_str().unwrap();
        let uri = format!("/api/projects/{id}/facts");
        let bank = format!("project:{id}");
        // A conversation already said it.
        let said = f.said(&bank, "The deploy is on Friday", "");

        let (status, added) = f
            .call(
                Method::POST,
                &uri,
                Some(json!({ "text": "the deploy is on friday" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{added}");
        assert_eq!(added["existing"], true);
        assert_eq!(added["fact"]["id"], said.as_str());
        assert_eq!(
            added["fact"]["source"], "manual",
            "kept as typed from now on"
        );
        assert_eq!(f.store().count(&bank).unwrap(), 1);
    }

    #[tokio::test]
    async fn a_fact_added_by_hand_gets_a_vector_when_a_model_is_configured() {
        let f = fixture();
        let p = f.create("Sen", &f.folder("sen")).await;
        let id = p["id"].as_str().unwrap();
        let engine = Arc::new(FactEngine::new(
            f.ctx.db.clone().unwrap(),
            Some(Arc::new(Bag)),
        ));
        let (status, added) = f
            .facts_call(
                Some(engine.clone()),
                Method::POST,
                &format!("/api/projects/{id}/facts"),
                Some(json!({ "text": "Deploy to staging on Friday", "kind": "experience" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{added}");
        assert_eq!(added["fact"]["kind"], "experience");
        let near = engine
            .store
            .search_vector(
                &format!("project:{id}"),
                "bag:v1:3",
                &[1.0, 1.0, 1.0],
                0.5,
                5,
                Scope::Agent,
            )
            .unwrap();
        assert_eq!(
            near.len(),
            1,
            "stored with its vector and the model's identity"
        );
        assert_eq!(near[0].0.text, "Deploy to staging on Friday");
    }

    #[tokio::test]
    async fn a_fact_that_is_not_a_fact_is_refused_and_nothing_is_stored() {
        let f = fixture();
        let p = f.create("Sen", &f.folder("sen")).await;
        let id = p["id"].as_str().unwrap();
        let uri = format!("/api/projects/{id}/facts");
        let long = "x".repeat(extract::MAX_FACT_CHARS + 1);
        let key = concat!("sk-proj-", "abcdefghijklmnopqrstuvwxyz");
        let secret = format!("The staging key is {key} and rotates monthly");
        for (body, code) in [
            (json!({ "text": "short" }), "invalid_fact"),
            (json!({ "text": "   " }), "invalid_fact"),
            (json!({ "text": long }), "invalid_fact"),
            (
                json!({ "text": "The deploy is on Friday", "kind": "rumour" }),
                "invalid_kind",
            ),
            (
                json!({ "text": "The deploy is on Friday", "when": "next week" }),
                "invalid_when",
            ),
            (json!({ "text": secret }), "secret_in_fact"),
        ] {
            let (status, got) = f.call(Method::POST, &uri, Some(body.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {got}");
            assert_eq!(got["error"], code, "{body}");
        }
        // A misspelt field is an error, not a field that is ignored.
        let (status, _) = f
            .call(
                Method::POST,
                &uri,
                Some(json!({ "text": "The deploy is on Friday", "origen": "x" })),
            )
            .await;
        assert!(status.is_client_error());
        let (_, list) = f.call(Method::GET, &uri, None).await;
        assert_eq!(list["total"], 0);

        let (status, body) = f
            .call(
                Method::POST,
                "/api/projects/prj_00000000/facts",
                Some(json!({ "text": "The deploy is on Friday" })),
            )
            .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::NOT_FOUND, Some("project_not_found"))
        );
    }

    #[tokio::test]
    async fn a_project_can_only_list_and_forget_its_own_facts() {
        let f = fixture();
        let a = f.create("Alpha", &f.folder("a")).await;
        let b = f.create("Beta", &f.folder("b")).await;
        let (a_id, b_id) = (a["id"].as_str().unwrap(), b["id"].as_str().unwrap());
        let theirs = f.said(&format!("project:{b_id}"), "Beta ships on Monday", "");
        let agents = f.said("main", "Sen lives in Hue", "");

        let (_, list) = f
            .call(Method::GET, &format!("/api/projects/{a_id}/facts"), None)
            .await;
        assert_eq!(list["count"], 0, "Beta's fact is not Alpha's");
        for fact_id in [&theirs, &agents] {
            let (status, _) = f
                .call(
                    Method::DELETE,
                    &format!("/api/projects/{a_id}/facts/{fact_id}"),
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{fact_id}");
        }
        assert_eq!(f.store().count(&format!("project:{b_id}")).unwrap(), 1);
        assert_eq!(f.store().count("main").unwrap(), 1);
    }

    #[tokio::test]
    async fn purging_a_project_deletes_its_bank_and_only_its_bank() {
        let f = fixture();
        let a = f.create("Alpha", &f.folder("a")).await;
        let b = f.create("Beta", &f.folder("b")).await;
        let (a_id, b_id) = (a["id"].as_str().unwrap(), b["id"].as_str().unwrap());
        let a_bank = format!("project:{a_id}");
        // More than one slice of facts, so the deletion takes several passes.
        let n = crate::memory::engine::facts::store::DELETE_BATCH + 10;
        f.ctx
            .db
            .clone()
            .unwrap()
            .with_conn_mut(|c| {
                let tx = c.transaction()?;
                for i in 0..n {
                    tx.execute(
                        "INSERT INTO mem_facts (id, bank, kind, text, mentioned_at, created_at, content_hash) \
                         VALUES (?1, ?2, 'world', 'x', 1, 1, ?1)",
                        rusqlite::params![format!("f{i}"), a_bank],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .unwrap();
        f.said(&format!("project:{b_id}"), "Beta ships on Monday", "");
        f.said("main", "Sen lives in Hue", "web:main:1");

        // Archiving keeps every fact: it hides the project, it forgets nothing.
        let (status, _) = f
            .call(Method::DELETE, &format!("/api/projects/{a_id}"), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(f.store().count(&a_bank).unwrap(), n);

        let (status, gone) = f
            .call(
                Method::DELETE,
                &format!("/api/projects/{a_id}?purge=true"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{gone}");
        assert_eq!(gone["deletedFacts"], n);
        assert_eq!(f.store().count(&a_bank).unwrap(), 0);
        assert_eq!(f.store().count(&format!("project:{b_id}")).unwrap(), 1);
        assert_eq!(f.store().count("main").unwrap(), 1);
    }

    #[tokio::test]
    async fn purging_a_project_that_does_not_exist_deletes_no_facts() {
        let f = fixture();
        f.said("project:prj_0a1b2c3d", "Someone's orphaned fact", "");
        let (status, _) = f
            .call(
                Method::DELETE,
                "/api/projects/prj_0a1b2c3d?purge=true",
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(f.store().count("project:prj_0a1b2c3d").unwrap(), 1);
    }
}
