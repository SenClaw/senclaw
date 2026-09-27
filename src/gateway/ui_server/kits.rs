//! `/api/kits` — install, inspect and remove Zen Kits.
//!
//! The daemon owns the install now, so every client (desktop UI, Flutter app,
//! CLI) gets the same behaviour instead of each re-implementing the ordering,
//! the skip-don't-overwrite rule, and the receipt. See [`crate::kits`].
//!
//! | route | does |
//! |---|---|
//! | `GET  /api/kits` | kits installed on this daemon, from the receipt ledger |
//! | `POST /api/kits/preview` | parse + validate a manifest, report warnings, install nothing |
//! | `POST /api/kits/install` | install, returning a per-item report |
//! | `DELETE /api/kits/:id` | remove what the receipt says this kit created |

use std::sync::Arc;

use axum::{
    extract::{FromRequest, Path as AxumPath, Request, State},
    http::StatusCode,
    response::Json,
};

use axum_extra::extract::Multipart;

use crate::kits::{
    bundle::KitBundleError, install_bundle_with_params, kit_app_ids, uninstall_kit_with_extra,
    KitBundle, KitInstallReport,
    resolve_values, KitContext, KitManifest,
    KitManifestError, KitParamValues, KitReceiptStore,
};

use super::core::{AppError, UiState};

fn bad(msg: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.into())
}

/// Build the installer's view of this daemon's directories.
fn context(s: &UiState) -> KitContext<'_> {
    let paths = &s.config.paths;
    KitContext {
        virtual_agents_dir: paths.virtual_agents_dir.clone(),
        managed_skills_dir: paths.managed_skills_dir.clone(),
        workflows_dir: paths.workflows_dir.clone(),
        kits_dir: paths.kits_dir.clone(),
        patterns_dir: paths.patterns_dir.clone(),
        db: s.db.as_deref(),
    }
}

/// Accept either a bare manifest or `{"manifest": {...}}` / `{"kit": {...}}`.
///
/// Clients differ on the wrapper and getting it wrong is an infuriating 400 to
/// debug, so take both rather than making the caller guess.
fn manifest_from_body(body: &serde_json::Value) -> Result<KitManifest, AppError> {
    let raw = body
        .get("manifest")
        .filter(|v| v.is_object())
        .or_else(|| body.get("kit").filter(|v| v.is_object()))
        .unwrap_or(body);

    KitManifest::parse(raw).map_err(|e| match e {
        // "your app is too old" deserves its own status: it is not a malformed
        // request, and a client can tell the user to update instead of
        // reporting a parse failure.
        KitManifestError::TooNew { .. } => {
            AppError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
        }
        other => bad(other.to_string()),
    })
}

/// Pull the **answers** to the kit's declared params out of the request body.
///
/// `params` is deliberately overloaded on the wire: inside a manifest it is the
/// *declaration array*, and in the install wrapper it is the *answer object*.
/// Keying on the JSON type is what keeps `{"manifest": {…}, "params": {…}}`
/// working without breaking a bare manifest that carries its own `params: []` —
/// the same shape trick `manifest_from_body` uses for the wrapper itself.
fn param_values_from_body(body: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    body.get("params")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

/// Validate answers against the declarations, turning a failure into a 400 that
/// names every offending field at once.
fn resolve_params(
    kit: &KitManifest,
    body: &serde_json::Value,
) -> Result<KitParamValues, AppError> {
    resolve_values(&kit.params, &param_values_from_body(body)).map_err(|e| bad(e.to_string()))
}

/// What one `preview`/`install` call carries: the kit, plus the JSON envelope
/// its answers and flags travel in.
struct KitRequest {
    bundle: KitBundle,
    /// `{"params": {...}, "force": bool}` — an empty object for a bare upload.
    body: serde_json::Value,
}

impl KitRequest {
}

fn bundle_error(e: KitBundleError) -> AppError {
    match e {
        // Same distinction the JSON path draws: "your app is too old" is not a
        // malformed request, and a client can act on it.
        KitBundleError::Manifest(KitManifestError::TooNew { .. }) => {
            AppError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
        }
        other => bad(other.to_string()),
    }
}

/// Decode a request that may be either a JSON manifest or an uploaded bundle.
///
/// Both endpoints take both shapes because they are the same operation on the
/// same object — a kit — and which form it arrived in is the client's business,
/// not the API's. A `.json` uploaded through the file field is read as a
/// manifest rather than refused for not being a zip: it is what the user
/// picked, and the two are interchangeable everywhere else.
async fn kit_request(s: &Arc<UiState>, req: Request) -> Result<KitRequest, AppError> {
    let is_multipart = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("multipart/"));

    if !is_multipart {
        let Json(body) = Json::<serde_json::Value>::from_request(req, s)
            .await
            .map_err(|e| bad(format!("expected a JSON manifest or a multipart upload: {e}")))?;
        let manifest = manifest_from_body(&body)?;
        return Ok(KitRequest {
            bundle: KitBundle::from_manifest(manifest),
            body,
        });
    }

    let mut multipart = Multipart::from_request(req, s)
        .await
        .map_err(|e| bad(format!("invalid upload: {e}")))?;

    let mut file: Option<(String, Vec<u8>)> = None;
    let mut fields = serde_json::Map::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| bad(format!("invalid upload: {e}")))?
    {
        let name = field.name().map(str::to_string).unwrap_or_default();
        let filename = field.file_name().map(str::to_string);
        // The file may be named anything; treat a field that carries a filename
        // as the payload, so a client that names it `kit` or `bundle` works.
        if name == "file" || name == "kit" || name == "bundle" || filename.is_some() {
            let bytes = field
                .bytes()
                .await
                .map_err(|e| bad(format!("cannot read the uploaded file: {e}")))?;
            file = Some((filename.unwrap_or_default(), bytes.to_vec()));
        } else if let Ok(text) = field.text().await {
            // `params` arrives as a JSON string; everything else as a scalar.
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) => fields.insert(name, v),
                Err(_) => fields.insert(name, serde_json::Value::String(text)),
            };
        }
    }

    let (filename, bytes) = file.ok_or_else(|| {
        bad("upload the kit as a .zip or .json file in the multipart field `file`")
    })?;
    let body = serde_json::Value::Object(fields);

    Ok(KitRequest {
        bundle: bundle_from_artifact(&filename, &bytes)?,
        body,
    })
}

/// Read a kit artifact — a `.json` manifest or a `.zip` bundle — into a bundle.
///
/// The two are told apart by extension and, failing that, by the first
/// non-whitespace byte: a `.json` chosen in a file picker that only advertises
/// zips must not fail with "not a readable zip", which would send the user
/// hunting for a problem with a perfectly good file.
fn bundle_from_artifact(filename: &str, bytes: &[u8]) -> Result<KitBundle, AppError> {
    let looks_json = filename.to_ascii_lowercase().ends_with(".json")
        || bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
    if looks_json {
        let raw: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|e| bad(format!("{filename} is not valid JSON: {e}")))?;
        return Ok(KitBundle::from_manifest(manifest_from_body(&raw)?));
    }
    KitBundle::from_zip(bytes).map_err(bundle_error)
}

/// Counts of what would actually be installed — the merged view, not the
/// manifest's own lists. A bundle whose skills live in `skills/` would
/// otherwise preview as installing none.
fn preview_counts(bundle: &KitBundle) -> serde_json::Value {
    let kit = &bundle.manifest;
    serde_json::json!({
        "agents": kit.agents.len(),
        "skills": bundle.skill_sources().len(),
        "workflows": bundle.workflow_sources().len(),
        "hooks": kit.hooks.len(),
        "jobs": kit.jobs.len(),
        "mcpServers": kit.mcp_servers.len(),
        "apps": bundle.apps.len(),
    })
}

/// What travelled as files, so the dialog can show it apart from what the
/// manifest merely declares.
fn bundle_summary(bundle: &KitBundle) -> serde_json::Value {
    serde_json::json!({
        "hasFiles": bundle.has_files(),
        "skills": bundle.skills.keys().collect::<Vec<_>>(),
        "workflows": bundle.workflows.keys().collect::<Vec<_>>(),
        "apps": bundle
            .apps
            .iter()
            .map(|a| serde_json::json!({ "id": a.id, "bytes": a.zip.len() }))
            .collect::<Vec<_>>(),
    })
}

/// Từng mục kit sẽ cài, đủ để client dựng danh sách chi tiết thay vì chỉ đếm.
///
/// Trả về trường có cấu trúc (`cron`, `agentRef`, `description`…) chứ không phải
/// một câu đã ghép sẵn: nhãn đi kèm phải theo ngôn ngữ của client, và web với
/// desktop dịch riêng.
fn preview_items(bundle: &KitBundle) -> Vec<serde_json::Value> {
    let kit = &bundle.manifest;
    let mut out = Vec::new();

    for agent in &kit.agents {
        out.push(serde_json::json!({
            "type": "agent",
            "name": agent.name,
            "description": agent.description,
        }));
    }
    for (name, source) in bundle.skill_sources() {
        out.push(serde_json::json!({
            "type": "skill",
            "name": name,
            "description": kit
                .skills
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.description.clone())
                .filter(|d| !d.is_empty()),
            // Cùng một tên có thể vừa khai báo vừa có thư mục; nói rõ bản nào
            // thắng, vì thư mục mang theo script mà `content` không có.
            "source": match source {
                crate::kits::installer::SkillSource::Files(_) => "bundle",
                crate::kits::installer::SkillSource::Inline(_) => "manifest",
            },
        }));
    }
    for (name, _) in bundle.workflow_sources() {
        out.push(serde_json::json!({
            "type": "workflow",
            "name": name,
            "description": kit
                .workflows
                .iter()
                .find(|w| w.name == name)
                .and_then(|w| w.description.clone()),
            "source": if bundle.workflows.contains_key(name) { "bundle" } else { "manifest" },
        }));
    }
    for hook in &kit.hooks {
        out.push(serde_json::json!({
            "type": "hook",
            "name": hook.event,
            "matcher": hook.matcher,
            "if": hook.if_condition,
            "blocking": hook.blocking,
        }));
    }
    for job in &kit.jobs {
        out.push(serde_json::json!({
            "type": "job",
            "name": job.name,
            "cron": job.cron,
            "agentRef": job.agent_ref,
            // `false` = cài ở trạng thái tạm dừng; đáng nói trước, vì một lịch
            // bắt đầu chạy ngay khi vừa cài sẽ tiêu token trước khi ai kịp đọc.
            "enabled": job.enabled_on_install,
        }));
    }
    for app in &bundle.apps {
        out.push(serde_json::json!({
            "type": "app",
            "name": app.id,
            "bytes": app.zip.len(),
        }));
    }
    for (i, server) in kit.mcp_servers.iter().enumerate() {
        out.push(serde_json::json!({
            "type": "mcpServer",
            "name": server
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&format!("#{}", i + 1))
                .to_string(),
            "unsupported": true,
        }));
    }
    out
}

/// Thân trả lời chung cho cả hai đường xem trước (tệp tải lên và kit lấy từ
/// marketplace) — cùng một object nên chỉ dựng một chỗ.
fn preview_payload(
    s: &UiState,
    bundle: &KitBundle,
    param_error: Option<String>,
) -> serde_json::Value {
    let kit = &bundle.manifest;
    serde_json::json!({
        "id": kit.id,
        "name": kit.name,
        "version": kit.version,
        "description": kit.description,
        "manifest": kit.manifest,
        "params": kit.params,
        "paramError": param_error,
        "counts": preview_counts(bundle),
        "items": preview_items(bundle),
        "bundle": bundle_summary(bundle),
        "installed": KitReceiptStore::new(&s.config.paths.kits_dir)
            .get(&kit.id)
            .map(|r| serde_json::json!({ "version": r.version, "installedAt": r.installed_at })),
        "warnings": kit.warnings(),
    })
}

// ===== GET /api/kits =====

pub(crate) async fn kits_list(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let store = KitReceiptStore::new(&s.config.paths.kits_dir);
    Json(serde_json::json!({ "kits": store.list() }))
}

// ===== POST /api/kits/preview =====

/// Validate without touching anything: what a kit contains, what this build
/// would refuse, and what it would warn about.
pub(crate) async fn kits_preview(
    State(s): State<Arc<UiState>>,
    req: Request,
) -> Result<Json<serde_json::Value>, AppError> {
    let KitRequest { mut bundle, body } = kit_request(&s, req).await?;
    // Answers are optional here. When the client sends them, say whether they
    // would be accepted — that is what lets a form validate without the user
    // having to attempt an install to find out.
    let param_error = match resolve_params(&bundle.manifest, &body) {
        Ok(values) => {
            // Fill the answers in before listing the items. The install does the
            // same substitution, so a preview showing `{{param.team}}` would be
            // previewing something the user is never going to get.
            bundle.manifest.apply_params(&values);
            None
        }
        Err(AppError(_, message)) => Some(message),
    };
    Ok(Json(preview_payload(&s, &bundle, param_error)))
}

// ===== POST /api/kits/install =====

pub(crate) async fn kits_install(
    State(s): State<Arc<UiState>>,
    req: Request,
) -> Result<Json<serde_json::Value>, AppError> {
    let KitRequest { mut bundle, body } = kit_request(&s, req).await?;

    // Substitute before anything reaches disk: the installer must never see a
    // half-templated manifest, or the persona name it writes and the job that
    // points at it stop matching.
    let values = resolve_params(&bundle.manifest, &body)?;
    bundle.manifest.apply_params(&values);

    let mut report = install_bundle_with_params(&bundle, &context(&s), &values);

    // Apps last, and here rather than in the installer: installing one is an
    // async call that runs the Space App security scan, which the sync
    // installer has no way to make.
    let app_records = install_bundle_apps(&s, &bundle, &mut report, body.get("force")).await;
    if !app_records.is_empty() {
        append_receipt_items(&s, &bundle.manifest, app_records);
    }

    // Pattern sources the same way and for the same reason: the installer
    // registered them, but cloning a repo is network I/O it cannot do.
    sync_kit_pattern_sources(&s, &bundle.manifest, &mut report).await;

    // Newly installed personas are only real once the registry has looked
    // again.
    refresh_after_change(&s, report.created() > 0);

    Ok(Json(serde_json::json!({
        "ok": !report.any_failed(),
        "report": report,
    })))
}

/// Clone every git pattern source the kit declared with `syncOnInstall`.
///
/// Only sources this kit actually registered are fetched: a declaration whose
/// id was already taken was *skipped* by the installer (never-overwrite, so a
/// user's own fork survives a kit install), and pulling it here would fetch
/// into someone else's source.
///
/// A failed clone downgrades that one item rather than failing the install.
/// The source stays registered with its error recorded, so Plugins → Patterns
/// shows a retry button instead of the kit having to be reinstalled.
async fn sync_kit_pattern_sources(
    s: &Arc<UiState>,
    kit: &KitManifest,
    report: &mut KitInstallReport,
) {
    use crate::kits::installer::KitItemStatus;

    for decl in kit.pattern_sources.iter().filter(|d| d.sync_on_install) {
        let raw = if decl.id.trim().is_empty() {
            &kit.id
        } else {
            &decl.id
        };
        let Ok(id) = crate::patterns::sanitize_name(raw) else {
            continue;
        };
        let installed_here = report
            .items
            .iter()
            .any(|i| i.kind == "patternSource" && i.name == id && i.status == KitItemStatus::Created);
        if !installed_here {
            continue;
        }

        let item = report
            .items
            .iter_mut()
            .find(|i| i.kind == "patternSource" && i.name == id);
        match super::patterns::blocking_sync(s, &id).await {
            Ok(out) => {
                if let Some(item) = item {
                    let warn = if out.pinned {
                        String::new()
                    } else {
                        // Worth saying out loud: a pattern is used as a system
                        // prompt, so an unpinned source lets an upstream commit
                        // rewrite instructions the agent will obey.
                        " — tracking a branch, not a pinned tag".to_string()
                    };
                    item.detail = Some(format!("{} patterns downloaded{warn}", out.patterns));
                }
            }
            Err(e) => {
                if let Some(item) = item {
                    item.status = KitItemStatus::Failed;
                    item.detail = Some(format!("registered, but the download failed: {}", e.1));
                }
            }
        }
    }
}

/// Install every Space App that travelled in the bundle, appending one outcome
/// per app to the report and returning the receipt records for those that
/// landed.
///
/// A blocked scan is reported as a failed item carrying the findings, not as a
/// failed request: the rest of the kit installed, and the rule is that every
/// item reports an outcome. `force` re-runs the same call with the override the
/// Space Apps page uses.
async fn install_bundle_apps(
    s: &Arc<UiState>,
    bundle: &KitBundle,
    report: &mut KitInstallReport,
    force: Option<&serde_json::Value>,
) -> Vec<crate::kits::receipt::KitItemRecord> {
    use crate::kits::installer::{KitItemOutcome, KitItemStatus};

    let force = force.and_then(serde_json::Value::as_bool).unwrap_or(false);
    let mut records = Vec::new();

    for app in &bundle.apps {
        let outcome = super::space::install_app_from_zip(s.clone(), app.zip.clone(), None, force)
            .await;
        let item = match outcome {
            Ok(super::space::AppInstallOutcome::Installed(value)) => {
                let id = value
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(&app.id)
                    .to_string();
                records.push(crate::kits::receipt::KitItemRecord {
                    kind: crate::kits::KitItemKind::App,
                    name: id.clone(),
                    path: None,
                    engine_ref: Some(id.clone()),
                });
                KitItemOutcome::client_owned("app", id, KitItemStatus::Created)
            }
            Ok(super::space::AppInstallOutcome::Blocked(scan)) => {
                KitItemOutcome::client_owned("app", app.id.clone(), KitItemStatus::Failed)
                    .with_detail(format!(
                        "blocked by the pre-install security scan (risk {}/100); \
                         nothing was installed for this app",
                        scan.risk_score()
                    ))
            }
            Err(AppError(_, message)) => {
                KitItemOutcome::client_owned("app", app.id.clone(), KitItemStatus::Failed)
                    .with_detail(message)
            }
        };
        report.items.push(item);
    }
    records
}

/// Add records to a kit's receipt after the installer already wrote it.
///
/// The apps install after the receipt is saved, and a record that never lands
/// is an item uninstall will not remove — so this reads the receipt back and
/// merges rather than assuming the installer left one behind.
fn append_receipt_items(
    s: &Arc<UiState>,
    kit: &KitManifest,
    records: Vec<crate::kits::receipt::KitItemRecord>,
) {
    let store = KitReceiptStore::new(&s.config.paths.kits_dir);
    let mut receipt = store.get(&kit.id).unwrap_or_else(|| crate::kits::KitReceipt {
        id: kit.id.clone(),
        version: kit.version.clone(),
        name: kit.name.clone(),
        description: kit.description.clone(),
        installed_at: crate::kits::receipt::now_rfc3339(),
        items: Vec::new(),
        params: Default::default(),
    });
    for record in records {
        if !receipt
            .items
            .iter()
            .any(|r| r.kind == record.kind && r.name == record.name)
        {
            receipt.items.push(record);
        }
    }
    if let Err(e) = store.save(receipt) {
        tracing::error!("[kits] cannot record installed apps for {}: {e}", kit.id);
    }
}

// ===== GET /api/kits/available =====

/// Kits offered by the configured marketplace sources.
///
/// Best-effort by design: a source whose catalog is unreachable contributes
/// nothing and is named in `notes`, rather than failing the request and
/// emptying a list the other sources could still fill.
pub(crate) async fn kits_available(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let installed_now = KitReceiptStore::new(&s.config.paths.kits_dir).list();
    let builtin: Vec<serde_json::Value> = crate::kits::builtin::rows()
        .into_iter()
        .map(|row| {
            let existing = installed_now.iter().find(|r| r.id == row.id);
            let mut v = serde_json::to_value(&row).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "installedVersion".into(),
                    match existing {
                        Some(r) => serde_json::Value::String(r.version.clone()),
                        None => serde_json::Value::Null,
                    },
                );
            }
            v
        })
        .collect();

    let Some(manager) = s.marketplace_manager.as_ref() else {
        // A kit compiled into the binary is installable with or without a
        // marketplace, so it must still be offered here.
        return Ok(Json(serde_json::json!({
            "kits": builtin,
            "notes": ["marketplace is not configured"],
        })));
    };
    let offered = manager
        .lock()
        .map_err(|_| AppError(StatusCode::INTERNAL_SERVER_ERROR, "marketplace busy".into()))?
        .list_kits();

    let installed = KitReceiptStore::new(&s.config.paths.kits_dir).list();
    let kits: Vec<serde_json::Value> = builtin
        .into_iter()
        .chain(offered.into_iter()
        .map(|(source, kit)| {
            // Match on the declared id when the catalog gives one, else on the
            // name — a catalog that omits `id` still gets the badge right for
            // the common case where the two agree.
            let key = kit.id.clone().unwrap_or_else(|| kit.name.clone());
            let existing = installed.iter().find(|r| r.id == key);
            serde_json::json!({
                "sourceId": source.id,
                "sourceName": source.name,
                "name": kit.name,
                "id": kit.id,
                "description": kit.description,
                "version": kit.version,
                "author": kit.author,
                "keywords": kit.keywords,
                "category": kit.category,
                "homepage": kit.homepage,
                "installable": kit.url.is_some(),
                "installedVersion": existing.map(|r| r.version.clone()),
            })
        }))
        .collect();

    Ok(Json(serde_json::json!({ "kits": kits })))
}

// ===== POST /api/kits/available/preview | /install =====

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceKitBody {
    source_id: String,
    name: String,
    #[serde(default)]
    params: serde_json::Value,
    #[serde(default)]
    force: bool,
}

/// Pull a kit's artifact out of its source, whatever form it takes.
///
/// Built-ins are checked first and deliberately do not need the marketplace:
/// requiring a configured source before a kit that ships in the binary can be
/// installed would make the feature unavailable on a fresh machine.
fn fetch_source_kit(s: &UiState, body: &SourceKitBody) -> Result<KitBundle, AppError> {
    if body.source_id == crate::kits::builtin::BUILTIN_SOURCE {
        let kit = crate::kits::builtin::find(&body.name)
            .ok_or_else(|| bad(format!("no built-in kit \"{}\"", body.name)))?;
        return bundle_from_artifact("kit.json", kit.manifest.as_bytes());
    }
    let manager = s.marketplace_manager.as_ref().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "marketplace is not configured".into(),
        )
    })?;
    let (filename, bytes) = manager
        .lock()
        .map_err(|_| AppError(StatusCode::INTERNAL_SERVER_ERROR, "marketplace busy".into()))?
        .fetch_kit(&body.source_id, &body.name)
        .map_err(|e| bad(e.to_string()))?;
    bundle_from_artifact(&filename, &bytes)
}

/// The same preview a local file gets, for a kit that lives in a source.
pub(crate) async fn kits_available_preview(
    State(s): State<Arc<UiState>>,
    Json(body): Json<SourceKitBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut bundle = fetch_source_kit(&s, &body)?;
    let envelope = serde_json::json!({ "params": body.params });
    let param_error = match resolve_params(&bundle.manifest, &envelope) {
        Ok(values) => {
            bundle.manifest.apply_params(&values);
            None
        }
        Err(AppError(_, message)) => Some(message),
    };
    Ok(Json(preview_payload(&s, &bundle, param_error)))
}

pub(crate) async fn kits_available_install(
    State(s): State<Arc<UiState>>,
    Json(body): Json<SourceKitBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut bundle = fetch_source_kit(&s, &body)?;
    let envelope = serde_json::json!({ "params": body.params });

    let values = resolve_params(&bundle.manifest, &envelope)?;
    bundle.manifest.apply_params(&values);

    let mut report = install_bundle_with_params(&bundle, &context(&s), &values);
    let app_records = install_bundle_apps(
        &s,
        &bundle,
        &mut report,
        Some(&serde_json::Value::Bool(body.force)),
    )
    .await;
    if !app_records.is_empty() {
        append_receipt_items(&s, &bundle.manifest, app_records);
    }
    sync_kit_pattern_sources(&s, &bundle.manifest, &mut report).await;
    refresh_after_change(&s, report.created() > 0);

    Ok(Json(serde_json::json!({
        "ok": !report.any_failed(),
        "report": report,
    })))
}

// ===== DELETE /api/kits/:id =====

pub(crate) async fn kits_uninstall(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Apps first, for the same reason they install last: removing one is async.
    // Their outcomes are handed to the sync remover, which merges them into the
    // report and only drops the receipt when everything actually went.
    let app_outcomes = uninstall_bundle_apps(&s, &id).await;

    let Some(report) = uninstall_kit_with_extra(&id, &context(&s), app_outcomes) else {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("no installed kit \"{id}\""),
        ));
    };
    refresh_after_change(&s, true);

    Ok(Json(serde_json::json!({
        "ok": !report.any_failed(),
        "report": report,
    })))
}

/// Remove the Space Apps this kit installed, one outcome each.
async fn uninstall_bundle_apps(
    s: &Arc<UiState>,
    kit_id: &str,
) -> Vec<crate::kits::installer::KitRemoveOutcome> {
    use crate::kits::installer::{KitRemoveOutcome, KitRemoveStatus};

    let mut out = Vec::new();
    for app_id in kit_app_ids(kit_id, &context(s)) {
        let result =
            super::space::space_apps_delete(State(s.clone()), AxumPath(app_id.clone())).await;
        out.push(KitRemoveOutcome {
            kind: "app".into(),
            name: app_id.clone(),
            status: match &result {
                Ok(_) => KitRemoveStatus::Removed,
                // The app is already gone — the user removed it from the Space
                // Apps page. Not an error, and not a reason to keep the receipt.
                Err(AppError(StatusCode::NOT_FOUND, _)) => KitRemoveStatus::Missing,
                Err(_) => KitRemoveStatus::Failed,
            },
            detail: result.err().map(|AppError(_, m)| m),
        });
    }
    out
}

/// Re-read the caches a kit just invalidated.
fn refresh_after_change(s: &UiState, changed: bool) {
    if !changed {
        return;
    }
    if let Some(registry) = s.persona_registry.as_ref() {
        registry.lock().unwrap().reload();
    }
    // Hooks load once per engine, and engines are cached per chat — so without
    // this a kit's hook sits on disk inert in every existing conversation and
    // only starts firing after a daemon restart. The same call covers uninstall:
    // the file is gone, and a live engine must stop honouring it.
    if let Some(api) = s.agent_api.as_ref() {
        api.reload_all_hooks();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).unwrap()
    }

    /// `AppError` carries no `Debug`, so `unwrap()` on these results does not
    /// compile — unwrap by hand and report the daemon's own message instead.
    fn expect_kit(value: &serde_json::Value) -> KitManifest {
        match manifest_from_body(value) {
            Ok(kit) => kit,
            Err(AppError(status, message)) => {
                panic!("expected a usable manifest, got {status}: {message}")
            }
        }
    }

    #[test]
    fn accepts_a_bare_manifest_or_either_wrapper() {
        let inner = r#"{"manifest":2,"id":"k","agents":[{"name":"A","systemPrompt":"p"}]}"#;

        for shape in [
            inner.to_string(),
            format!(r#"{{"manifest":{inner}}}"#),
            format!(r#"{{"kit":{inner}}}"#),
        ] {
            let kit = expect_kit(&body(&shape));
            assert_eq!(kit.id, "k");
        }
    }

    #[test]
    fn a_too_new_manifest_is_not_reported_as_a_bad_request() {
        // 422 lets a client say "update the app"; 400 would send them hunting
        // for a mistake in a manifest that is perfectly well-formed.
        let err = manifest_from_body(&body(r#"{"manifest":99,"id":"k"}"#)).unwrap_err();
        assert_eq!(err.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err.1.contains("update SenClaw"));
    }

    #[test]
    fn a_broken_manifest_is_a_bad_request_with_the_reason() {
        let err = manifest_from_body(&body(r#"{"name":"no id"}"#)).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("id"));
    }

    #[test]
    fn answers_are_read_from_the_wrapper_not_from_a_manifests_own_params_list() {
        // `params` means two different things depending on where it sits: an
        // array of declarations inside the manifest, an object of answers in
        // the wrapper. Reading the array as answers would silently drop them.
        let bare = body(r#"{"manifest":2,"id":"k","params":[{"key":"a"}],"agents":[{"name":"A","systemPrompt":"p"}]}"#);
        assert!(param_values_from_body(&bare).is_empty());

        let wrapped = body(
            r#"{"manifest":{"manifest":2,"id":"k","agents":[{"name":"A","systemPrompt":"p"}]},
                 "params":{"a":"x"}}"#,
        );
        assert_eq!(param_values_from_body(&wrapped)["a"], "x");
    }

    #[test]
    fn a_missing_required_answer_is_a_bad_request_naming_the_field() {
        let value = body(
            r#"{"manifest":{"manifest":2,"id":"k","params":[{"key":"workdir","label":"Folder","required":true}],
                 "agents":[{"name":"A","systemPrompt":"in {{param.workdir}}"}]}}"#,
        );
        let kit = expect_kit(&value);
        let err = resolve_params(&kit, &value).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("Folder"), "{}", err.1);
    }

    #[test]
    fn answers_are_substituted_through_the_manifest_before_install() {
        let value = body(
            r#"{"manifest":{"manifest":2,"id":"k",
                 "params":[{"key":"who","default":"Zen"}],
                 "agents":[{"name":"{{param.who}} Bot","systemPrompt":"I am {{param.who}}"}],
                 "jobs":[{"name":"j","agentRef":"{{param.who}} Bot","cron":"0 9 * * *"}]}}"#,
        );
        let mut kit = expect_kit(&value);
        // `AppError` has no `Debug`, so unwrap by hand (see `expect_kit`).
        let values = match resolve_params(&kit, &value) {
            Ok(v) => v,
            Err(AppError(status, message)) => panic!("expected valid params, got {status}: {message}"),
        };
        kit.apply_params(&values);

        // The persona registry keys on the name and the job resolves against
        // it, so both sides must come out of the same pass.
        assert_eq!(kit.agents[0].name, "Zen Bot");
        assert_eq!(kit.agents[0].system_prompt, "I am Zen");
        assert_eq!(kit.jobs[0].agent_ref.as_deref(), Some("Zen Bot"));
    }

    #[test]
    fn wrapper_key_holding_a_non_object_falls_back_to_the_body() {
        // `{"manifest": 2, ...}` is a version field, not a wrapper — reading
        // it as one would reject every well-formed v2 manifest.
        let kit = expect_kit(&body(
            r#"{"manifest":2,"id":"k","jobs":[{"name":"j","cron":"0 9 * * *"}]}"#,
        ));
        assert_eq!(kit.id, "k");
        assert_eq!(kit.manifest, 2);
    }
}
