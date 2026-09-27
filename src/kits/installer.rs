//! Install and uninstall a kit against this daemon's own stores.
//!
//! Five item kinds land in four different places:
//!
//! | kind     | where it goes                                        |
//! |----------|------------------------------------------------------|
//! | agent    | `<virtual_agents_dir>/kit-<kit>__<name>.md`          |
//! | skill    | `<managed_skills_dir>/<name>/` + `.senclaw-kit.json` |
//! | workflow | `<workflows_dir>/<name>.md`                          |
//! | hook     | `<kits_dir>/hooks/<kit>.json`                        |
//! | job      | a `background_tasks` row owned by the kit             |
//! | pattern  | `<patterns_dir>/kit-<kit>/<name>/system.md`          |
//! | patternSource | a row in `<patterns_dir>/sources.json`           |
//!
//! Three rules, carried over from the client-side installer this replaces:
//!
//! 1. **Never overwrite.** An item whose name is already taken is skipped, not
//!    replaced — the user may have edited it, and a kit update must not undo
//!    that. Skipped items stay out of the receipt, so uninstall never deletes
//!    something this kit did not create.
//! 2. **Never stop halfway.** Every item produces an outcome even when an
//!    earlier one failed; a half-installed kit the user can see beats an
//!    opaque error.
//! 3. **Jobs point at the persona's declared name**, not a slugged filename —
//!    the persona registry keys on the front-matter `name:`, and a job that
//!    misses it runs with no persona while only logging a warning.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::db::Db;
use crate::types::{
    BackgroundContinuity, BackgroundJobKind, BackgroundOwnerKind, BackgroundPromptKind,
    BackgroundTask, BackgroundTaskStatus, BackgroundTrigger, BackgroundVisibility, OverlapPolicy,
};

use super::hooks::{write_kit_hooks, KitHookOutcome};
use super::params::KitParamValues;
use super::manifest::{safe_segment, KitManifest};
use super::bundle::{BundleFile, KitBundle};
use super::receipt::{now_rfc3339, KitItemKind, KitItemRecord, KitReceipt, KitReceiptStore};

/// Marker dropped next to a kit-installed skill, mirroring the Space App
/// bundle's `.senclaw-app.json`. It is what makes the directory removable
/// later without guessing from the name.
pub const KIT_SKILL_MARKER: &str = ".senclaw-kit.json";

/// What happened to one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KitItemStatus {
    Created,
    /// Something with that name was already there; left untouched.
    Skipped,
    /// The daemon does not install this kind (mcpServers, apps) — a client
    /// drives those through their own endpoints.
    Unsupported,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KitItemOutcome {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub status: KitItemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl KitItemOutcome {
    fn new(kind: KitItemKind, name: &str, status: KitItemStatus) -> Self {
        Self {
            kind: kind.as_str().to_string(),
            name: name.to_string(),
            status,
            detail: None,
        }
    }

    /// For declarations the daemon never creates — `mcpServers` and `apps`.
    ///
    /// They get their kind as a plain string because [`KitItemKind`] only names
    /// things the receipt can hold. Borrowing `Skill` for them, as this used to,
    /// made the install report state something untrue: the UI renders `type`
    /// directly, so an MCP server appeared to the user as a skill.
    pub(crate) fn client_owned(kind: &str, name: String, status: KitItemStatus) -> Self {
        Self {
            kind: kind.to_string(),
            name,
            status,
            detail: None,
        }
    }

    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KitInstallReport {
    pub kit_id: String,
    pub version: String,
    pub items: Vec<KitItemOutcome>,
    pub warnings: Vec<super::manifest::KitWarning>,
}

impl KitInstallReport {
    pub fn created(&self) -> usize {
        self.count(KitItemStatus::Created)
    }

    pub fn count(&self, status: KitItemStatus) -> usize {
        self.items.iter().filter(|i| i.status == status).count()
    }

    pub fn any_failed(&self) -> bool {
        self.items.iter().any(|i| i.status == KitItemStatus::Failed)
    }
}

/// Everything the installer is allowed to touch. Passing the pieces in (rather
/// than a whole `Config` + `UiState`) is what lets the tests run the real code
/// path against a temp dir with no database.
pub struct KitContext<'a> {
    pub virtual_agents_dir: PathBuf,
    pub managed_skills_dir: PathBuf,
    pub workflows_dir: PathBuf,
    pub kits_dir: PathBuf,
    /// Root of [`crate::patterns`] — where a kit's patterns and its registered
    /// pattern sources land.
    pub patterns_dir: PathBuf,
    /// `None` = no database available; jobs are reported as failed rather than
    /// silently dropped.
    pub db: Option<&'a Db>,
}

impl KitContext<'_> {
    pub fn receipts(&self) -> KitReceiptStore {
        KitReceiptStore::new(&self.kits_dir)
    }

    /// Persona filename for a kit-owned agent. The `kit-<id>__` prefix marks
    /// ownership only — the persona registers under its front-matter `name:`.
    fn persona_path(&self, kit_id: &str, agent_name: &str) -> PathBuf {
        self.virtual_agents_dir.join(format!(
            "kit-{}__{}.md",
            safe_segment(kit_id),
            safe_segment(agent_name)
        ))
    }

    fn skill_dir(&self, skill_name: &str) -> PathBuf {
        self.managed_skills_dir.join(safe_segment(skill_name))
    }

    fn workflow_path(&self, name: &str) -> PathBuf {
        self.workflows_dir.join(format!("{}.md", safe_segment(name)))
    }
}

/// Install `kit`, writing a receipt of what was created.
///
/// The receipt is saved even when some items failed: what landed on disk is
/// still on disk, and losing that record is what makes an install unremovable.
pub fn install_kit(kit: &KitManifest, ctx: &KitContext<'_>) -> KitInstallReport {
    install_kit_with_params(kit, ctx, &KitParamValues::new())
}

/// As [`install_kit`], recording the answers the user gave for the kit's
/// declared params.
///
/// The manifest reaching here is already substituted — `apply_params` runs
/// before anything is written — so these values are for the receipt alone, and
/// secret ones never enter it.
pub fn install_kit_with_params(
    kit: &KitManifest,
    ctx: &KitContext<'_>,
    param_values: &KitParamValues,
) -> KitInstallReport {
    install_bundle_with_params(&KitBundle::from_manifest(kit.clone()), ctx, param_values)
}

/// Install a zip bundle: the manifest's own items plus the skills, workflows
/// and apps that travelled as files beside it.
///
/// Apps are deliberately **not** installed here. Installing one is an async
/// call into the Space App installer (download-free, but it runs a security
/// scan and touches `UiState`), and this function is sync and `UiState`-free so
/// its tests can run the real path against a temp dir. The HTTP layer installs
/// the apps and appends their outcomes to this report.
pub fn install_bundle_with_params(
    bundle: &KitBundle,
    ctx: &KitContext<'_>,
    param_values: &KitParamValues,
) -> KitInstallReport {
    let kit = &bundle.manifest;
    let mut items = Vec::new();
    let mut records: Vec<KitItemRecord> = Vec::new();

    // Agents first: jobs below resolve `agentRef` against what exists now.
    for agent in &kit.agents {
        let path = ctx.persona_path(&kit.id, &agent.name);
        if persona_name_taken(&ctx.virtual_agents_dir, &agent.name) {
            items.push(
                KitItemOutcome::new(KitItemKind::Agent, &agent.name, KitItemStatus::Skipped)
                    .with_detail("a persona with that name already exists"),
            );
            continue;
        }
        match write_file(&path, &KitManifest::persona_markdown(agent)) {
            Ok(()) => {
                records.push(KitItemRecord {
                    kind: KitItemKind::Agent,
                    name: agent.name.clone(),
                    path: Some(path.display().to_string()),
                    engine_ref: None,
                });
                items.push(KitItemOutcome::new(
                    KitItemKind::Agent,
                    &agent.name,
                    KitItemStatus::Created,
                ));
            }
            Err(e) => items.push(
                KitItemOutcome::new(KitItemKind::Agent, &agent.name, KitItemStatus::Failed)
                    .with_detail(e.to_string()),
            ),
        }
    }

    // Skills come from two places: inlined in the manifest, or shipped as a
    // directory in a zip bundle. Union them by name; a name in both installs
    // from the bundle, because a directory carries scripts and references that
    // a single `content` string never could.
    for (name, source) in bundle.skill_sources() {
        let dir = ctx.skill_dir(name);
        if dir.exists() {
            items.push(
                KitItemOutcome::new(KitItemKind::Skill, name, KitItemStatus::Skipped)
                    .with_detail("a skill directory with that name already exists"),
            );
            continue;
        }
        let marker = serde_json::json!({ "kit_id": kit.id, "skill": name });
        let result = match source {
            SkillSource::Inline(skill) => {
                write_file(&dir.join("SKILL.md"), &KitManifest::skill_markdown(skill))
            }
            SkillSource::Files(files) => write_skill_files(&dir, files, param_values),
        }
        .and_then(|()| {
            write_file(
                &dir.join(KIT_SKILL_MARKER),
                &serde_json::to_string_pretty(&marker).unwrap_or_default(),
            )
        });
        match result {
            Ok(()) => {
                records.push(KitItemRecord {
                    kind: KitItemKind::Skill,
                    name: name.to_string(),
                    path: Some(dir.display().to_string()),
                    engine_ref: None,
                });
                items.push(KitItemOutcome::new(
                    KitItemKind::Skill,
                    name,
                    KitItemStatus::Created,
                ));
            }
            Err(e) => {
                // A half-written skill directory would be picked up by the
                // loader as a broken skill and, having never entered the
                // receipt, would survive uninstall. Take it back out.
                let _ = std::fs::remove_dir_all(&dir);
                items.push(
                    KitItemOutcome::new(KitItemKind::Skill, name, KitItemStatus::Failed)
                        .with_detail(e.to_string()),
                );
            }
        }
    }

    for (name, content) in bundle.workflow_sources() {
        let path = ctx.workflow_path(name);
        if path.exists() {
            items.push(
                KitItemOutcome::new(KitItemKind::Workflow, name, KitItemStatus::Skipped)
                    .with_detail("a workflow with that name already exists"),
            );
            continue;
        }
        let workflow_name = name;
        match write_file(&path, &super::params::substitute(content, param_values)) {
            Ok(()) => {
                records.push(KitItemRecord {
                    kind: KitItemKind::Workflow,
                    name: workflow_name.to_string(),
                    path: Some(path.display().to_string()),
                    engine_ref: None,
                });
                items.push(KitItemOutcome::new(
                    KitItemKind::Workflow,
                    workflow_name,
                    KitItemStatus::Created,
                ));
            }
            Err(e) => items.push(
                KitItemOutcome::new(KitItemKind::Workflow, workflow_name, KitItemStatus::Failed)
                    .with_detail(e.to_string()),
            ),
        }
    }

    install_patterns(kit, ctx, &mut items, &mut records);

    // Hooks are one file per kit, so "install" is a single write and
    // "uninstall" is a single delete — the user's own hooks.json is never
    // touched by a kit.
    if !kit.hooks.is_empty() {
        // Same rule as every other item: an existing file is left alone. The
        // hook file is kit-owned, but a user may well have tuned the prompt in
        // it, and a reinstall must not quietly undo that.
        if super::hooks::kit_hook_path(&ctx.kits_dir, &kit.id).exists() {
            items.push(
                KitItemOutcome::new(KitItemKind::Hook, &kit.id, KitItemStatus::Skipped)
                    .with_detail("this kit's hook file already exists"),
            );
        } else {
            match write_kit_hooks(&ctx.kits_dir, kit) {
                KitHookOutcome::Written { path, accepted } => {
                    records.push(KitItemRecord {
                        kind: KitItemKind::Hook,
                        name: kit.id.clone(),
                        path: Some(path.display().to_string()),
                        engine_ref: None,
                    });
                    items.push(
                        KitItemOutcome::new(KitItemKind::Hook, &kit.id, KitItemStatus::Created)
                            .with_detail(format!("{accepted} hook(s) registered")),
                    );
                }
                KitHookOutcome::Rejected(reason) => items.push(
                    KitItemOutcome::new(KitItemKind::Hook, &kit.id, KitItemStatus::Failed)
                        .with_detail(reason),
                ),
            }
        }
    }

    for job in &kit.jobs {
        let outcome = install_job(kit, job, ctx, &mut records);
        items.push(outcome);
    }

    // Declared but not the daemon's to install — say so instead of dropping
    // them on the floor.
    for (i, server) in kit.mcp_servers.iter().enumerate() {
        items.push(
            KitItemOutcome::client_owned(
                "mcpServer",
                blob_label(server, "name", i),
                KitItemStatus::Unsupported,
            )
            .with_detail("install MCP servers through /api/mcp-servers"),
        );
    }
    // An app declared in the manifest but not shipped in the bundle has no
    // files to install from — the manifest entry is a reference, not a payload.
    // Apps that DID travel in the bundle are installed by the HTTP layer, which
    // appends their outcomes to this report.
    for (i, app) in kit.apps.iter().enumerate() {
        let label = blob_label(app, "id", i);
        if bundle.apps.iter().any(|a| a.id == label) {
            continue;
        }
        items.push(
            KitItemOutcome::client_owned("app", label, KitItemStatus::Unsupported)
                .with_detail(
                    "declared but not shipped in the bundle — install it from Space Apps, \
                     or ship it as apps/<id>.zip inside the kit",
                ),
        );
    }

    if !records.is_empty() {
        let previous = ctx.receipts().get(&kit.id);
        // Merge with an earlier install so a resumed/partial install keeps the
        // record of what the first attempt created.
        let mut merged = previous.map(|p| p.items).unwrap_or_default();
        for record in records {
            if !merged
                .iter()
                .any(|r| r.kind == record.kind && r.name == record.name)
            {
                merged.push(record);
            }
        }
        let receipt = KitReceipt {
            id: kit.id.clone(),
            version: kit.version.clone(),
            name: kit.name.clone(),
            description: kit.description.clone(),
            installed_at: now_rfc3339(),
            items: merged,
            // Secrets never, and blanks are noise: an optional param nobody
            // answered resolves to "" and would otherwise show up as an empty
            // chip in "installed with".
            params: param_values
                .iter()
                .filter(|(_, v)| !v.secret && !v.text.is_empty())
                .map(|(k, v)| (k.clone(), v.text.clone()))
                .collect(),
        };
        if let Err(e) = ctx.receipts().save(receipt) {
            tracing::error!("[kits] cannot write receipt for {}: {e}", kit.id);
        }
    }

    KitInstallReport {
        kit_id: kit.id.clone(),
        version: kit.version.clone(),
        items,
        warnings: kit.warnings(),
    }
}

/// Where one skill's content comes from.
pub(crate) enum SkillSource<'a> {
    /// `content` inlined in the manifest.
    Inline(&'a super::manifest::KitSkill),
    /// A directory that travelled in the zip bundle.
    Files(&'a [BundleFile]),
}

impl KitBundle {
    /// Every skill this bundle installs, by name. A name present both inline
    /// and as files resolves to the files.
    pub(crate) fn skill_sources(&self) -> Vec<(&str, SkillSource<'_>)> {
        let mut out: Vec<(&str, SkillSource<'_>)> = Vec::new();
        for skill in &self.manifest.skills {
            match self.skills.get(&skill.name) {
                Some(files) => out.push((skill.name.as_str(), SkillSource::Files(files))),
                None => out.push((skill.name.as_str(), SkillSource::Inline(skill))),
            }
        }
        // Directories the manifest never mentioned still install: in a bundle
        // the files are the declaration.
        for (name, files) in &self.skills {
            if !self.manifest.skills.iter().any(|s| s.name == *name) {
                out.push((name.as_str(), SkillSource::Files(files)));
            }
        }
        out
    }

    /// Every workflow this bundle installs, by name, files winning over inline.
    pub(crate) fn workflow_sources(&self) -> Vec<(&str, &str)> {
        let mut out: Vec<(&str, &str)> = Vec::new();
        for wf in &self.manifest.workflows {
            let content = self
                .workflows
                .get(&wf.name)
                .map(String::as_str)
                .unwrap_or(wf.content.as_str());
            out.push((wf.name.as_str(), content));
        }
        for (name, content) in &self.workflows {
            if !self.manifest.workflows.iter().any(|w| w.name == *name) {
                out.push((name.as_str(), content.as_str()));
            }
        }
        out
    }
}

/// Write a skill directory out of the bundle.
///
/// Every path was already vetted by the zip reader (`enclosed_name` rejects
/// anything escaping the archive root), and each is re-checked here against the
/// destination: this function writes wherever it is told, so it must not be the
/// one place that trusts its input.
fn write_skill_files(
    dir: &Path,
    files: &[BundleFile],
    values: &KitParamValues,
) -> std::io::Result<()> {
    for file in files {
        let target = dir.join(&file.rel);
        if !target.starts_with(dir) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} escapes the skill directory", file.rel),
            ));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, substitute_bytes(&file.bytes, values))?;
    }
    Ok(())
}

/// Substitute `{{param.<key>}}` in a bundle file, leaving binary files alone.
///
/// A manifest's own strings go through the same substitution before anything is
/// written, and a file shipped beside it is no different: an author who wrote
/// `{{param.team}}` in their `SKILL.md` meant it to be filled in, and shipping
/// the placeholder verbatim to the user is the failure. Non-UTF-8 files (an
/// image in a skill directory) pass through untouched — rewriting those would
/// corrupt them.
fn substitute_bytes(bytes: &[u8], values: &KitParamValues) -> Vec<u8> {
    match std::str::from_utf8(bytes) {
        Ok(text) => super::params::substitute(text, values).into_bytes(),
        Err(_) => bytes.to_vec(),
    }
}

fn install_job(
    kit: &KitManifest,
    job: &super::manifest::KitJob,
    ctx: &KitContext<'_>,
    records: &mut Vec<KitItemRecord>,
) -> KitItemOutcome {
    let Some(db) = ctx.db else {
        return KitItemOutcome::new(KitItemKind::Job, &job.name, KitItemStatus::Failed)
            .with_detail("no database available");
    };

    let owner_key = format!("{}::{}", safe_segment(&kit.id), safe_segment(&job.name));
    match db.get_background_task_by_key(&kit.id, &owner_key) {
        Ok(Some(_)) => {
            return KitItemOutcome::new(KitItemKind::Job, &job.name, KitItemStatus::Skipped)
                .with_detail("this kit already installed a job with that name");
        }
        Ok(None) => {}
        Err(e) => {
            return KitItemOutcome::new(KitItemKind::Job, &job.name, KitItemStatus::Failed)
                .with_detail(e.to_string());
        }
    }

    let now = now_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();
    let mut task = BackgroundTask {
        id: id.clone(),
        // `App` + `owner_id = <kit id>` reuses the existing owner index and
        // `delete_background_tasks_by_owner`, which is exactly the sweep an
        // uninstall needs.
        owner_kind: BackgroundOwnerKind::App,
        owner_id: kit.id.clone(),
        owner_key,
        title: job.name.clone(),
        description: None,
        job_kind: BackgroundJobKind::Prompt,
        native_job: None,
        prompt_kind: BackgroundPromptKind::Static,
        prompt: Some(if job.input.trim().is_empty() {
            job.name.clone()
        } else {
            job.input.clone()
        }),
        context_url: None,
        // Verbatim: the persona registry keys on the declared name.
        persona: job.agent_ref.clone().filter(|p| !p.trim().is_empty()),
        agent_folder: None,
        workspace_dir: None,
        use_tools: Vec::new(),
        mcp_json: None,
        model_id: None,
        max_turns: None,
        timeout_secs: None,
        continuity: BackgroundContinuity::Fresh,
        memory_folder: None,
        trigger_type: BackgroundTrigger::Cron,
        trigger_value: Some(job.cron.clone()),
        next_run: None,
        last_run: None,
        overlap_policy: OverlapPolicy::Skip,
        catch_up: false,
        max_failures: job.max_failures.unwrap_or(5),
        consecutive_failures: 0,
        visibility: BackgroundVisibility::Normal,
        notify: false,
        status: if job.enabled_on_install {
            BackgroundTaskStatus::Active
        } else {
            BackgroundTaskStatus::Paused
        },
        created_at: now.clone(),
        updated_at: now,
    };

    // A cron the scheduler cannot read would sit in the table forever without
    // firing, so refuse it here where the message can still reach the user.
    task.next_run = crate::background::plan_next_run(&task, chrono::Utc::now());
    if task.next_run.is_none() && task.status == BackgroundTaskStatus::Active {
        return KitItemOutcome::new(KitItemKind::Job, &job.name, KitItemStatus::Failed)
            .with_detail(format!("'{}' is not a valid cron expression", job.cron));
    }

    match db.upsert_background_task(&task) {
        Ok(()) => {
            records.push(KitItemRecord {
                kind: KitItemKind::Job,
                name: job.name.clone(),
                path: None,
                engine_ref: Some(id),
            });
            KitItemOutcome::new(KitItemKind::Job, &job.name, KitItemStatus::Created)
        }
        Err(e) => KitItemOutcome::new(KitItemKind::Job, &job.name, KitItemStatus::Failed)
            .with_detail(e.to_string()),
    }
}

// ============================================================================
// Uninstall
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KitRemoveStatus {
    Removed,
    /// Already gone — the user deleted it by hand. Not an error.
    Missing,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KitRemoveOutcome {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub status: KitRemoveStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KitUninstallReport {
    pub kit_id: String,
    pub items: Vec<KitRemoveOutcome>,
}

impl KitUninstallReport {
    pub fn any_failed(&self) -> bool {
        self.items
            .iter()
            .any(|i| i.status == KitRemoveStatus::Failed)
    }
}

/// Remove everything the receipt says this kit created, then drop the receipt.
///
/// The receipt is kept when anything failed: it is the only record of what is
/// still out there, and deleting it would strand those items for good.
pub fn uninstall_kit(kit_id: &str, ctx: &KitContext<'_>) -> Option<KitUninstallReport> {
    uninstall_kit_with_extra(kit_id, ctx, Vec::new())
}

/// The ids of the Space Apps this kit installed, so a caller that *can* remove
/// them (the HTTP layer — app removal is async) knows what to take out before
/// calling [`uninstall_kit_with_extra`].
pub fn kit_app_ids(kit_id: &str, ctx: &KitContext<'_>) -> Vec<String> {
    ctx.receipts()
        .get(kit_id)
        .map(|r| {
            r.items
                .iter()
                .filter(|i| i.kind == KitItemKind::App)
                .filter_map(|i| i.engine_ref.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// As [`uninstall_kit`], merging in outcomes the caller already produced.
///
/// Apps are the reason this exists: removing one is an async call the sync
/// installer cannot make, so the HTTP layer removes them first and passes the
/// outcomes in. An app record with no matching outcome is reported as failed
/// rather than skipped — the app is still on disk, and a silent skip would let
/// the receipt be deleted while it stays behind forever.
pub fn uninstall_kit_with_extra(
    kit_id: &str,
    ctx: &KitContext<'_>,
    extra: Vec<KitRemoveOutcome>,
) -> Option<KitUninstallReport> {
    let receipt = ctx.receipts().get(kit_id)?;
    let mut items = Vec::new();

    for record in &receipt.items {
        let outcome = match record.kind {
            KitItemKind::Job => remove_job(ctx, record),
            KitItemKind::Skill => remove_path(record, true),
            KitItemKind::Pattern => remove_path(record, true),
            KitItemKind::PatternSource => remove_pattern_source(ctx, record),
            KitItemKind::App => {
                let id = record.engine_ref.as_deref().unwrap_or(&record.name);
                match extra.iter().find(|o| o.name == id || o.name == record.name) {
                    Some(done) => done.clone(),
                    None => KitRemoveOutcome {
                        kind: "app".into(),
                        name: record.name.clone(),
                        status: KitRemoveStatus::Failed,
                        detail: Some(
                            "a Space App can only be removed through the app installer; \
                             remove it from Space Apps"
                                .into(),
                        ),
                    },
                }
            }
            _ => remove_path(record, false),
        };
        items.push(outcome);
    }

    let report = KitUninstallReport {
        kit_id: kit_id.to_string(),
        items,
    };
    if !report.any_failed() {
        if let Err(e) = ctx.receipts().remove(kit_id) {
            tracing::error!("[kits] removed {kit_id} but could not update the receipt: {e}");
        }
    }
    Some(report)
}

fn remove_path(record: &KitItemRecord, is_dir: bool) -> KitRemoveOutcome {
    let out = |status, detail: Option<String>| KitRemoveOutcome {
        kind: record.kind.as_str().to_string(),
        name: record.name.clone(),
        status,
        detail,
    };
    let Some(path) = record.path.as_ref().map(PathBuf::from) else {
        return out(KitRemoveStatus::Failed, Some("no path recorded".into()));
    };
    if !path.exists() {
        return out(KitRemoveStatus::Missing, None);
    }
    let result = if is_dir {
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    };
    match result {
        Ok(()) => out(KitRemoveStatus::Removed, None),
        Err(e) => out(KitRemoveStatus::Failed, Some(e.to_string())),
    }
}

/// Write the kit's inline patterns and register the git sources it declares.
///
/// Inline patterns go into a **kit-owned source** (`kit-<id>`) rather than the
/// user's own. Two reasons: uninstall becomes a directory delete that cannot
/// take a hand-written pattern with it, and a kit pattern never silently wins
/// over one the user wrote, because the user source is always resolved first
/// ([`crate::patterns::registry`]).
///
/// Git sources are only **registered** here. Cloning them is network I/O and
/// belongs in the HTTP layer, for the same reason Space App installs do — see
/// `kits_install`.
fn install_patterns(
    kit: &KitManifest,
    ctx: &KitContext<'_>,
    items: &mut Vec<KitItemOutcome>,
    records: &mut Vec<KitItemRecord>,
) {
    use crate::patterns::{PatternSource, PatternStore, SourceKind};

    if kit.patterns.is_empty() && kit.pattern_sources.is_empty() {
        return;
    }
    let store = PatternStore::new(&ctx.patterns_dir);

    if !kit.patterns.is_empty() {
        let owned = PatternSource::for_kit(&kit.id);
        // Registering the source is what makes the patterns visible at all; a
        // failure here means the writes below would land in a directory nobody
        // scans, so say so once instead of once per pattern.
        if let Err(e) = store.upsert_source(owned.clone()) {
            items.push(
                KitItemOutcome::new(KitItemKind::PatternSource, &owned.id, KitItemStatus::Failed)
                    .with_detail(e.to_string()),
            );
        } else {
            let first_install = !records
                .iter()
                .any(|r| r.kind == KitItemKind::PatternSource && r.name == owned.id);
            if first_install {
                records.push(KitItemRecord {
                    kind: KitItemKind::PatternSource,
                    name: owned.id.clone(),
                    path: None,
                    engine_ref: Some(owned.id.clone()),
                });
            }

            for pattern in &kit.patterns {
                match store.write(
                    &owned,
                    &pattern.name,
                    &pattern.system,
                    pattern.user.as_deref(),
                    false,
                ) {
                    Ok(files) => {
                        records.push(KitItemRecord {
                            kind: KitItemKind::Pattern,
                            name: files.name.clone(),
                            path: Some(files.path.clone()),
                            engine_ref: None,
                        });
                        items.push(KitItemOutcome::new(
                            KitItemKind::Pattern,
                            &files.name,
                            KitItemStatus::Created,
                        ));
                    }
                    Err(crate::patterns::StoreError::Exists(name)) => items.push(
                        KitItemOutcome::new(KitItemKind::Pattern, &name, KitItemStatus::Skipped)
                            .with_detail("a pattern with that name already exists in this kit"),
                    ),
                    Err(e) => items.push(
                        KitItemOutcome::new(
                            KitItemKind::Pattern,
                            &pattern.name,
                            KitItemStatus::Failed,
                        )
                        .with_detail(e.to_string()),
                    ),
                }
            }
        }
    }

    for decl in &kit.pattern_sources {
        let id = match crate::patterns::sanitize_name(if decl.id.trim().is_empty() {
            &kit.id
        } else {
            &decl.id
        }) {
            Ok(id) => id,
            Err(e) => {
                items.push(
                    KitItemOutcome::new(KitItemKind::PatternSource, &decl.id, KitItemStatus::Failed)
                        .with_detail(e.to_string()),
                );
                continue;
            }
        };
        // Never-overwrite applies here too: a source the user already added
        // may point at their own fork, and a reinstall must not redirect it.
        if store.source(&id).is_ok() {
            items.push(
                KitItemOutcome::new(KitItemKind::PatternSource, &id, KitItemStatus::Skipped)
                    .with_detail("a pattern source with that id already exists"),
            );
            continue;
        }
        let source = PatternSource {
            name: if decl.name.trim().is_empty() {
                id.clone()
            } else {
                decl.name.clone()
            },
            kind: SourceKind::Git,
            url: Some(decl.url.clone()),
            git_ref: if decl.git_ref.trim().is_empty() {
                "main".to_string()
            } else {
                decl.git_ref.clone()
            },
            subdir: decl.subdir.clone(),
            strategies_subdir: decl.strategies_subdir.clone(),
            enabled: true,
            installed_by: Some(format!("kit:{}", kit.id)),
            last_synced_at: None,
            last_error: None,
            id: id.clone(),
        };
        match store.upsert_source(source) {
            Ok(()) => {
                records.push(KitItemRecord {
                    kind: KitItemKind::PatternSource,
                    name: id.clone(),
                    path: None,
                    engine_ref: Some(id.clone()),
                });
                items.push(
                    KitItemOutcome::new(KitItemKind::PatternSource, &id, KitItemStatus::Created)
                        .with_detail(if decl.sync_on_install {
                            "registered — downloading patterns"
                        } else {
                            "registered — sync it from Plugins → Patterns to download"
                        }),
                );
            }
            Err(e) => items.push(
                KitItemOutcome::new(KitItemKind::PatternSource, &id, KitItemStatus::Failed)
                    .with_detail(e.to_string()),
            ),
        }
    }
}

/// De-register a pattern source and delete whatever it brought down.
fn remove_pattern_source(ctx: &KitContext<'_>, record: &KitItemRecord) -> KitRemoveOutcome {
    let out = |status, detail: Option<String>| KitRemoveOutcome {
        kind: record.kind.as_str().to_string(),
        name: record.name.clone(),
        status,
        detail,
    };
    let id = record.engine_ref.as_deref().unwrap_or(&record.name);
    let store = crate::patterns::PatternStore::new(&ctx.patterns_dir);
    if store.source(id).is_err() {
        return out(KitRemoveStatus::Missing, None);
    }
    match store.remove_source(id) {
        Ok(()) => out(KitRemoveStatus::Removed, None),
        Err(e) => out(KitRemoveStatus::Failed, Some(e.to_string())),
    }
}

fn remove_job(ctx: &KitContext<'_>, record: &KitItemRecord) -> KitRemoveOutcome {
    let out = |status, detail: Option<String>| KitRemoveOutcome {
        kind: record.kind.as_str().to_string(),
        name: record.name.clone(),
        status,
        detail,
    };
    let Some(db) = ctx.db else {
        return out(KitRemoveStatus::Failed, Some("no database available".into()));
    };
    let Some(id) = record.engine_ref.as_deref() else {
        return out(KitRemoveStatus::Failed, Some("no task id recorded".into()));
    };
    match db.get_background_task(id) {
        Ok(None) => out(KitRemoveStatus::Missing, None),
        Ok(Some(_)) => match db.delete_background_task(id) {
            Ok(()) => out(KitRemoveStatus::Removed, None),
            Err(e) => out(KitRemoveStatus::Failed, Some(e.to_string())),
        },
        Err(e) => out(KitRemoveStatus::Failed, Some(e.to_string())),
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// True when a persona with this front-matter `name:` already exists in the
/// directory, whoever put it there.
///
/// Checked by reading the files rather than by filename: the registry keys on
/// the front-matter name, so `alice.md` declaring `name: Zen Reporter` is a
/// collision even though the filenames differ.
fn persona_name_taken(dir: &Path, name: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        if front_matter_name(&body).is_some_and(|n| n == name) {
            return true;
        }
    }
    false
}

/// First `name:` inside the leading `---` block.
fn front_matter_name(body: &str) -> Option<String> {
    let mut lines = body.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            return None;
        }
        if let Some(rest) = trimmed.strip_prefix("name:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Display name for an opaque declaration the daemon does not parse.
///
/// `mcpServers` and `apps` are `serde_json::Value` on purpose — their schema
/// belongs to another subsystem — so the best available label is whatever
/// identifying key they conventionally carry, and a position when they carry
/// none. Reporting every one of them as the same word would make a kit with
/// three MCP servers look like it declared one.
fn blob_label(value: &serde_json::Value, key: &str, index: usize) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("#{}", index + 1))
}

fn write_file(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kits::manifest::{KitAgent, KitHook, KitJob, KitSkill, KitWorkflow};

    fn ctx(dir: &Path) -> KitContext<'static> {
        KitContext {
            virtual_agents_dir: dir.join("virtual-agents"),
            managed_skills_dir: dir.join("skills"),
            workflows_dir: dir.join("workflows"),
            kits_dir: dir.join("kits"),
            patterns_dir: dir.join("patterns"),
            db: None,
        }
    }

    fn kit() -> KitManifest {
        KitManifest {
            manifest: 2,
            id: "demo".into(),
            name: "Demo".into(),
            version: "1.0.0".into(),
            agents: vec![KitAgent {
                name: "Zen Reporter".into(),
                description: Some("d".into()),
                system_prompt: "body".into(),
                tools: vec![],
                max_concurrent: None,
            }],
            skills: vec![KitSkill {
                name: "report-format".into(),
                description: "d".into(),
                content: "# body".into(),
                triggers: vec![],
            }],
            workflows: vec![KitWorkflow {
                name: "morning".into(),
                description: None,
                content: "---\nname: morning\n---\nbody\n".into(),
            }],
            hooks: vec![KitHook {
                event: "SessionStart".into(),
                matcher: None,
                if_condition: None,
                prompt: "say hi".into(),
                timeout: None,
                blocking: false,
            }],
            ..Default::default()
        }
    }

    /// End to end over the real write path: answers reach the files, and the
    /// receipt keeps the plain ones while dropping the secret.
    #[test]
    fn params_are_substituted_into_the_files_and_recorded_without_secrets() {
        use crate::kits::manifest::KitManifest as M;
        use crate::kits::params::resolve_values;

        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());

        let raw = serde_json::json!({
            "manifest": 2,
            "id": "demo",
            "params": [
                { "key": "who", "default": "Zen" },
                { "key": "token", "secret": true, "default": "s3cret" }
            ],
            "agents": [{
                "name": "{{param.who}} Reporter",
                "systemPrompt": "I am {{param.who}}, key {{param.token}}"
            }],
            "skills": [{ "name": "fmt", "content": "# for {{param.who}}" }],
        });
        let mut kit = M::parse(&raw).unwrap();
        // No answers supplied — the declared defaults carry it, exactly like a
        // client that submitted the form untouched.
        let values = resolve_values(&kit.params, &serde_json::Map::new()).unwrap();
        kit.apply_params(&values);

        let report = install_kit_with_params(&kit, &ctx, &values);
        assert!(!report.any_failed(), "{:?}", report.items);

        // The persona filename is derived from the substituted name.
        let persona = fs::read_to_string(ctx.persona_path("demo", "Zen Reporter")).unwrap();
        assert!(persona.contains("name: Zen Reporter"), "{persona}");
        assert!(persona.contains("I am Zen, key s3cret"), "{persona}");
        assert!(
            !persona.contains("{{param."),
            "no placeholder may survive onto disk: {persona}"
        );

        let skill = fs::read_to_string(ctx.skill_dir("fmt").join("SKILL.md")).unwrap();
        assert!(skill.contains("# for Zen"), "{skill}");

        let receipt = ctx.receipts().get("demo").unwrap();
        assert_eq!(receipt.params.get("who").map(String::as_str), Some("Zen"));
        assert!(
            !receipt.params.contains_key("token"),
            "a secret must never reach installed.json: {:?}",
            receipt.params
        );
    }

    #[test]
    fn installs_every_file_backed_item_and_records_them() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());

        let report = install_kit(&kit(), &ctx);

        assert_eq!(report.created(), 4, "agent + skill + workflow + hooks");
        assert!(!report.any_failed());

        assert!(ctx
            .virtual_agents_dir
            .join("kit-demo__Zen-Reporter.md")
            .exists());
        assert!(ctx
            .managed_skills_dir
            .join("report-format/SKILL.md")
            .exists());
        assert!(ctx
            .managed_skills_dir
            .join("report-format")
            .join(KIT_SKILL_MARKER)
            .exists());
        assert!(ctx.workflows_dir.join("morning.md").exists());
        assert!(ctx.kits_dir.join("hooks/demo.json").exists());

        let receipt = ctx.receipts().get("demo").unwrap();
        assert_eq!(receipt.items.len(), 4);
        assert_eq!(receipt.version, "1.0.0");
    }

    #[test]
    fn persona_registers_under_the_declared_name() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        install_kit(&kit(), &ctx);

        let body =
            fs::read_to_string(ctx.virtual_agents_dir.join("kit-demo__Zen-Reporter.md")).unwrap();

        // The filename is slugged for the filesystem's sake; the name the
        // registry (and every job) uses must survive intact.
        assert!(body.contains("name: Zen Reporter\n"));
    }

    #[test]
    fn reinstalling_skips_instead_of_overwriting_user_edits() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        install_kit(&kit(), &ctx);

        let persona = ctx.virtual_agents_dir.join("kit-demo__Zen-Reporter.md");
        fs::write(&persona, "---\nname: Zen Reporter\n---\nEDITED BY USER\n").unwrap();

        let hooks = ctx.kits_dir.join("hooks/demo.json");
        fs::write(&hooks, "{\"hooks\":{\"Stop\":[]}} // tuned by hand").unwrap();

        let report = install_kit(&kit(), &ctx);

        assert_eq!(
            report.count(KitItemStatus::Skipped),
            4,
            "agent, skill, workflow and the hook file are all left alone"
        );
        assert!(fs::read_to_string(&persona)
            .unwrap()
            .contains("EDITED BY USER"));
        assert!(fs::read_to_string(&hooks).unwrap().contains("tuned by hand"));
    }

    #[test]
    fn a_persona_added_by_hand_blocks_the_kit_from_claiming_that_name() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        fs::create_dir_all(&ctx.virtual_agents_dir).unwrap();
        // Different filename, same registered name — still a collision.
        fs::write(
            ctx.virtual_agents_dir.join("mine.md"),
            "---\nname: Zen Reporter\n---\nmine\n",
        )
        .unwrap();

        let report = install_kit(&kit(), &ctx);

        let agent = report.items.iter().find(|i| i.kind == "agent").unwrap();
        assert_eq!(agent.status, KitItemStatus::Skipped);
        assert!(!ctx
            .virtual_agents_dir
            .join("kit-demo__Zen-Reporter.md")
            .exists());
    }

    #[test]
    fn skipped_items_stay_out_of_the_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        fs::create_dir_all(&ctx.workflows_dir).unwrap();
        fs::write(ctx.workflows_dir.join("morning.md"), "user's own\n").unwrap();

        install_kit(&kit(), &ctx);

        let receipt = ctx.receipts().get("demo").unwrap();
        assert!(
            !receipt
                .items
                .iter()
                .any(|i| i.kind == KitItemKind::Workflow),
            "uninstall must never delete a workflow this kit did not create"
        );
    }

    #[test]
    fn uninstall_removes_exactly_what_was_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        install_kit(&kit(), &ctx);

        let report = uninstall_kit("demo", &ctx).unwrap();

        assert!(!report.any_failed());
        assert!(report
            .items
            .iter()
            .all(|i| i.status == KitRemoveStatus::Removed));
        assert!(!ctx
            .virtual_agents_dir
            .join("kit-demo__Zen-Reporter.md")
            .exists());
        assert!(!ctx.managed_skills_dir.join("report-format").exists());
        assert!(!ctx.workflows_dir.join("morning.md").exists());
        assert!(!ctx.kits_dir.join("hooks/demo.json").exists());
        assert!(ctx.receipts().get("demo").is_none());
    }

    #[test]
    fn uninstall_treats_already_deleted_items_as_done() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        install_kit(&kit(), &ctx);
        fs::remove_file(ctx.workflows_dir.join("morning.md")).unwrap();

        let report = uninstall_kit("demo", &ctx).unwrap();

        let workflow = report.items.iter().find(|i| i.kind == "workflow").unwrap();
        assert_eq!(workflow.status, KitRemoveStatus::Missing);
        assert!(!report.any_failed(), "a thing already gone is not a failure");
    }

    #[test]
    fn uninstalling_an_unknown_kit_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(uninstall_kit("nope", &ctx(dir.path())).is_none());
    }

    #[test]
    fn jobs_need_a_database_and_say_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut k = kit();
        k.jobs = vec![KitJob {
            name: "morning".into(),
            agent_ref: Some("Zen Reporter".into()),
            cron: "0 9 * * *".into(),
            input: "go".into(),
            max_failures: None,
            enabled_on_install: true,
        }];

        let report = install_kit(&k, &ctx(dir.path()));

        let job = report.items.iter().find(|i| i.kind == "job").unwrap();
        assert_eq!(job.status, KitItemStatus::Failed);
        assert!(job.detail.as_ref().unwrap().contains("database"));
    }

    #[test]
    fn declared_but_client_owned_items_are_reported_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut k = kit();
        k.mcp_servers = vec![serde_json::json!({"name": "github"})];
        k.apps = vec![serde_json::json!({"id": "email"})];

        let report = install_kit(&k, &ctx(dir.path()));

        assert_eq!(report.count(KitItemStatus::Unsupported), 2);

        // The UI renders `type` verbatim, so these must name what they are —
        // reporting an MCP server as a "skill" tells the user something untrue.
        let unsupported: Vec<(&str, &str)> = report
            .items
            .iter()
            .filter(|i| i.status == KitItemStatus::Unsupported)
            .map(|i| (i.kind.as_str(), i.name.as_str()))
            .collect();
        assert_eq!(
            unsupported,
            vec![("mcpServer", "github"), ("app", "email")]
        );
    }

    #[test]
    fn a_nameless_client_owned_declaration_is_labelled_by_position() {
        // Three unnamed servers must not all render as the same row.
        let dir = tempfile::tempdir().unwrap();
        let mut k = kit();
        k.mcp_servers = vec![serde_json::json!({}), serde_json::json!({"name": "  "})];

        let report = install_kit(&k, &ctx(dir.path()));

        let names: Vec<&str> = report
            .items
            .iter()
            .filter(|i| i.kind == "mcpServer")
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(names, vec!["#1", "#2"]);
    }

    #[test]
    fn front_matter_name_only_reads_the_leading_block() {
        assert_eq!(
            front_matter_name("---\nname: A\n---\nbody"),
            Some("A".into())
        );
        assert_eq!(front_matter_name("no front matter\nname: A\n"), None);
        assert_eq!(front_matter_name("---\ndescription: d\n---\nname: A"), None);
    }

    /// Một tệp trong gói phải được thay `{{param.x}}` y như chuỗi trong
    /// manifest. Không thì tác giả viết `{{param.team}}` vào SKILL.md và người
    /// dùng nhận đúng chữ đó — hỏng một cách im lặng.
    #[test]
    fn bundle_files_get_the_same_substitution_as_the_manifest() {
        let values = crate::kits::params::resolve_values(
            &[crate::kits::KitParam {
                key: "team".into(),
                ..Default::default()
            }],
            &serde_json::json!({ "team": "Nền tảng" })
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();

        let text = substitute_bytes("báo cáo cho {{param.team}}".as_bytes(), &values);
        assert_eq!(String::from_utf8(text).unwrap(), "báo cáo cho Nền tảng");
    }

    /// Ảnh trong thư mục skill phải đi qua nguyên vẹn — viết lại nó là làm hỏng.
    #[test]
    fn a_binary_bundle_file_is_left_byte_for_byte() {
        let values = KitParamValues::new();
        let png = [0x89u8, b'P', b'N', b'G', 0xFF, 0xFE, 0x00, 0x01];
        assert_eq!(substitute_bytes(&png, &values), png.to_vec());
    }


    // ===== patterns =====

    #[test]
    fn inline_patterns_land_in_a_kit_owned_source_not_the_users() {
        use crate::patterns::{PatternRegistry, PatternStore, USER_SOURCE_ID};

        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let mut k = kit();
        k.patterns = vec![crate::kits::manifest::KitPattern {
            name: "Summarize Meeting".into(),
            description: "…".into(),
            system: "# IDENTITY\n\nSummarise a meeting.".into(),
            user: None,
        }];

        let report = install_kit(&k, &c);
        assert!(!report.any_failed(), "{report:?}");

        let store = PatternStore::new(&c.patterns_dir);
        let owned = store.source(&format!("kit-{}", k.id)).unwrap();
        // Name folded to a slug, and written to the kit's source.
        assert_eq!(store.names_in(&owned), vec!["summarize_meeting"]);
        // The user's own source stays empty: a kit never writes into it.
        assert!(store.names_in(&crate::patterns::PatternSource::user()).is_empty());

        // …and the pattern is still resolvable, because the kit source is
        // registered in the ledger.
        let reg = PatternRegistry::new(&store);
        let (src, files) = reg.resolve("summarize_meeting").unwrap();
        assert_ne!(src.id, USER_SOURCE_ID);
        assert!(files.system.contains("Summarise a meeting."));
    }

    #[test]
    fn uninstalling_takes_the_patterns_and_the_source_back_out() {
        use crate::patterns::PatternStore;

        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let mut k = kit();
        k.patterns = vec![crate::kits::manifest::KitPattern {
            name: "p1".into(),
            description: String::new(),
            system: "# H\n\nbody".into(),
            user: None,
        }];
        install_kit(&k, &c);

        let report = uninstall_kit(&k.id, &c).unwrap();
        assert!(!report.any_failed(), "{report:?}");

        let store = PatternStore::new(&c.patterns_dir);
        assert!(store.source(&format!("kit-{}", k.id)).is_err());
        assert!(!c.patterns_dir.join(format!("kit-{}", k.id)).exists());
    }

    #[test]
    fn a_git_pattern_source_is_registered_but_never_cloned_here() {
        use crate::patterns::{PatternStore, SourceKind};

        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let mut k = kit();
        k.pattern_sources = vec![crate::kits::manifest::KitPatternSource {
            id: "fabric".into(),
            name: "Fabric".into(),
            url: "https://github.com/danielmiessler/fabric".into(),
            git_ref: "main".into(),
            subdir: "data/patterns".into(),
            strategies_subdir: Some("data/strategies".into()),
            sync_on_install: true,
        }];

        let report = install_kit(&k, &c);
        assert!(!report.any_failed(), "{report:?}");

        let store = PatternStore::new(&c.patterns_dir);
        let src = store.source("fabric").unwrap();
        assert_eq!(src.kind, SourceKind::Git);
        assert_eq!(src.subdir, "data/patterns");
        assert_eq!(src.installed_by, Some(format!("kit:{}", k.id)));
        // The installer does no network I/O — that is the HTTP layer's job, so
        // an offline `cargo test` stays offline.
        assert!(!store.checkout_dir("fabric").exists());
    }

    #[test]
    fn a_pattern_source_the_user_already_added_is_never_redirected() {
        use crate::patterns::{PatternSource, PatternStore, SourceKind};

        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let store = PatternStore::new(&c.patterns_dir);
        store
            .upsert_source(PatternSource {
                id: "fabric".into(),
                kind: SourceKind::Git,
                url: Some("https://github.com/me/my-fork".into()),
                ..PatternSource::for_kit("mine")
            })
            .unwrap();

        let mut k = kit();
        k.pattern_sources = vec![crate::kits::manifest::KitPatternSource {
            id: "fabric".into(),
            name: String::new(),
            url: "https://github.com/danielmiessler/fabric".into(),
            git_ref: "main".into(),
            subdir: "data/patterns".into(),
            strategies_subdir: None,
            sync_on_install: true,
        }];
        install_kit(&k, &c);

        assert_eq!(
            store.source("fabric").unwrap().url.as_deref(),
            Some("https://github.com/me/my-fork"),
            "never-overwrite must protect a user's fork"
        );
    }

    #[test]
    fn a_kit_of_only_patterns_is_installable() {
        // `item_count` gates the whole parse: patterns had to be counted or a
        // pattern-only kit (which is exactly what the Fabric kit is) would be
        // rejected as declaring nothing.
        let raw = serde_json::json!({
            "manifest": 2,
            "id": "fabric",
            "patternSources": [
                { "id": "fabric", "url": "https://github.com/danielmiessler/fabric" }
            ]
        });
        let parsed = KitManifest::parse(&raw).unwrap();
        assert_eq!(parsed.pattern_sources.len(), 1);
        assert!(parsed.pattern_sources[0].sync_on_install, "default is on");
    }
}
