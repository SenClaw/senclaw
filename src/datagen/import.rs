//! `import`: bring an existing dataset — written and labelled by people —
//! into `datasets/` in the shape the pipelines read, with where it came
//! from and under what license recorded in its manifest.
//!
//! Four sources:
//! - `{"catalog": "<id>"}` — an entry of [`super::catalog`]: bundled (offline)
//!   or a pinned Hugging Face preset, mapping and license already written;
//! - `{"hf": {"dataset", "config"?, "split"?}}` — any public Hugging Face
//!   dataset the viewer API serves;
//! - `{"url": "https://…"}` — a JSONL / JSON / CSV / TSV file;
//! - `{"file": {"name", "content"}}` — the same, sent inline by a UI or by the
//!   MCP server reading a path in the chat's folder.
//!
//! No model is called. A row is dropped, with its reason counted, when a
//! required field is missing or too long, when the label map has no SenClaw
//! label for it, when the dataset viewer cut it short, or when it repeats a
//! record already written. What it records about its source (`provenance`)
//! travels with the dataset, because CC BY and BY-SA require the credit to
//! follow every reuse.

use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::catalog::{self, EntrySource, Flags};
use super::generator::AcceptLoop;
use super::hf::{self, HfClient};
use super::jobs::{JobCtx, Outcome};
use super::mapping::{self, FieldSource, Kind, LabelMap};
use super::tabular::{self, Format};
use super::url_fetch;

pub const DEFAULT_MAX_RECORDS: u32 = 20_000;
pub const MAX_RECORDS: u32 = 100_000;
/// Largest inline file; the request carrying it is capped at 8 MB.
pub const MAX_INLINE_BYTES: usize = 8 * 1024 * 1024;
pub const PREVIEW_ROWS: usize = 20;
const PUBLISH_EVERY: u32 = 200;
/// Pause between dataset viewer pages, so a long split does not run into
/// its rate limit in the first place.
const PAGE_PAUSE: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Source {
    Catalog(String),
    Hf(HfRef),
    Url(String),
    File(InlineFile),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HfRef {
    pub dataset: String,
    /// Default: the first config the viewer lists.
    #[serde(default)]
    pub config: Option<String>,
    /// Default: the first split of that config.
    #[serde(default)]
    pub split: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlineFile {
    /// The file's name; its extension picks the format.
    pub name: String,
    /// Never written to the manifest: the records are the copy that counts,
    /// and the provenance keeps its sha256.
    #[serde(default, skip_serializing)]
    pub content: String,
}

/// A bundled label map by id, or one given inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LabelMapRef {
    Named(String),
    Inline(BTreeMap<String, Option<String>>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Params {
    pub source: Source,
    /// Required unless the source is a catalog entry.
    #[serde(default)]
    pub kind: Option<Kind>,
    /// Field → column. Empty = guessed from the column names.
    #[serde(default)]
    pub mapping: BTreeMap<String, FieldSource>,
    #[serde(default)]
    pub label_map: Option<LabelMapRef>,
    #[serde(default)]
    pub max_records: Option<u32>,
    /// The source's license, or "unknown" / "private". Required unless the
    /// source is a catalog entry.
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub attribution: Option<String>,
    /// The person agreed to a catalog entry's terms (share-alike,
    /// evaluation-only…).
    #[serde(default)]
    pub accept_license: bool,
}

/// The import as it will run.
pub struct Plan {
    pub kind: Kind,
    pub mapping: BTreeMap<String, FieldSource>,
    pub labels: Option<LabelMap>,
    pub license: String,
    pub attribution: Option<String>,
    pub flags: Flags,
    pub catalog: Option<&'static catalog::Entry>,
    pub max_records: u32,
}

impl Params {
    pub fn validate(&self) -> Result<(), String> {
        self.plan().map(|_| ())
    }

    pub fn plan(&self) -> Result<Plan, String> {
        let max_records = self.max_records.unwrap_or(DEFAULT_MAX_RECORDS);
        if !(1..=MAX_RECORDS).contains(&max_records) {
            return Err(format!("maxRecords must be between 1 and {MAX_RECORDS}"));
        }
        if let Source::Catalog(id) = &self.source {
            let e = catalog::entry(id).ok_or_else(|| {
                let ids: Vec<&str> = catalog::entries().iter().map(|e| e.id.as_str()).collect();
                format!("no catalog entry \"{id}\" (entries: {})", ids.join(", "))
            })?;
            if self.kind.is_some() || !self.mapping.is_empty() || self.label_map.is_some() || self.license.is_some() {
                return Err("a catalog entry brings its own kind, mapping, label map and license — leave them out".into());
            }
            if e.flags.needs_consent() && !self.accept_license {
                return Err(format!(
                    "{} is {}: {} Ask the person, then send acceptLicense: true.",
                    e.id,
                    e.license,
                    e.notice.as_deref().unwrap_or("its terms need agreeing to.")
                ));
            }
            let labels = match &e.label_map {
                Some(id) => Some(catalog::label_map(id).cloned().ok_or_else(|| format!("no label map {id}"))?),
                None => None,
            };
            return Ok(Plan {
                kind: e.kind,
                mapping: e.mapping.clone(),
                labels,
                license: e.license.clone(),
                attribution: Some(e.attribution.clone()),
                flags: e.flags,
                catalog: Some(e),
                max_records,
            });
        }
        match &self.source {
            Source::Hf(r) => {
                if !hf::valid_dataset_id(r.dataset.trim()) {
                    return Err(format!("\"{}\" is not a Hugging Face dataset id (owner/name)", r.dataset));
                }
                for part in [&r.config, &r.split].into_iter().flatten() {
                    if !hf::valid_part(part) {
                        return Err(format!("\"{part}\" is not a config or split name"));
                    }
                }
            }
            Source::Url(u) => url_fetch::check(u.trim())?,
            Source::File(f) => {
                if f.name.trim().is_empty() {
                    return Err("the file needs a name".into());
                }
                if f.content.trim().is_empty() {
                    return Err(format!("{} is empty", f.name));
                }
                if f.content.len() > MAX_INLINE_BYTES {
                    return Err(format!("{} is larger than {} MB — give a URL instead", f.name, MAX_INLINE_BYTES / 1024 / 1024));
                }
            }
            Source::Catalog(_) => unreachable!("handled above"),
        }
        let kind = self.kind.ok_or("say which kind of records this is: labeled_text, qa_context, problem_answer or chat")?;
        if !self.mapping.is_empty() {
            mapping::check_mapping(kind, &self.mapping)?;
        }
        let license = self
            .license
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .ok_or("say the dataset's license (or \"unknown\" / \"private\") — it is kept with the records")?
            .to_string();
        let labels = match &self.label_map {
            None => None,
            Some(LabelMapRef::Named(id)) => Some(catalog::label_map(id).cloned().ok_or_else(|| format!("no label map {id}"))?),
            Some(LabelMapRef::Inline(map)) => Some(LabelMap { map: map.clone(), ..LabelMap::default() }),
        };
        if labels.is_some() && kind != Kind::LabeledText {
            return Err("a label map only applies to labeled_text".into());
        }
        Ok(Plan {
            kind,
            mapping: self.mapping.clone(),
            labels,
            license,
            attribution: self.attribution.clone().filter(|a| !a.trim().is_empty()),
            flags: Flags::default(),
            catalog: None,
            max_records,
        })
    }

    /// The default dataset name: the catalog id, the HF dataset or the
    /// file's stem.
    pub fn default_name(&self) -> String {
        match &self.source {
            Source::Catalog(id) => id.clone(),
            Source::Hf(r) => r.dataset.rsplit('/').next().unwrap_or(&r.dataset).to_string(),
            Source::Url(u) => stem(u.split(['?', '#']).next().unwrap_or(u).rsplit('/').next().unwrap_or("import")),
            Source::File(f) => stem(&f.name),
        }
    }
}

fn stem(name: &str) -> String {
    let s = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    if s.is_empty() {
        "import".into()
    } else {
        s.to_string()
    }
}

/// Rows read from a local source: a bundled file, an inline file or a
/// download, with the format they were read as and the sha256 of the bytes.
struct LocalRows {
    rows: Vec<Value>,
    /// A CSV / TSV header, in the file's order.
    header: Option<Vec<String>>,
    /// Every row in the source, kept or not.
    total: usize,
    format: Format,
    sha256: String,
}

fn read_local(name: &str, content_type: Option<&str>, text: &str, keep: usize) -> Result<LocalRows, String> {
    let format = Format::from_name(name)
        .or_else(|| match content_type {
            Some("application/json") => Some(Format::Json),
            Some("application/x-ndjson" | "application/jsonl" | "application/jsonlines") => Some(Format::Jsonl),
            Some("text/csv") => Some(Format::Csv),
            Some("text/tab-separated-values") => Some(Format::Tsv),
            _ => None,
        })
        .unwrap_or_else(|| Format::sniff(text));
    let (rows, total) = tabular::parse(text, format, keep)?;
    let header = tabular::header(text, format);
    Ok(LocalRows { rows, header, total, format, sha256: hex::encode(Sha256::digest(text.as_bytes())) })
}

/// Turns rows into records: maps, checks, drops duplicates, writes, counts.
struct Sink {
    kind: Kind,
    mapping: BTreeMap<String, FieldSource>,
    labels: Option<LabelMap>,
    seen: HashSet<u64>,
    hasher: Sha256,
    l: AcceptLoop,
}

impl Sink {
    fn full(&self) -> bool {
        self.l.accepted >= self.l.target
    }

    /// Settle the mapping from the first rows when none was given.
    fn settle_mapping(&mut self, sample: &[Value]) -> Result<(), String> {
        if !self.mapping.is_empty() {
            return Ok(());
        }
        let cols = tabular::columns(sample);
        let (_, guess) = mapping::suggest(&cols, Some(self.kind));
        mapping::check_mapping(self.kind, &guess)
            .map_err(|e| format!("{e} — no column name gave it away (columns: {}); send a mapping", cols.join(", ")))?;
        self.mapping = guess;
        Ok(())
    }

    fn push(&mut self, index: u64, row: Option<&Value>, ctx: &mut JobCtx) -> Result<(), String> {
        self.l.attempts += 1;
        let Some(row) = row else {
            self.l.reject("cut_by_viewer");
            return Ok(());
        };
        match mapping::normalize(row, index, self.kind, &self.mapping, self.labels.as_ref()) {
            Err(reason) => self.l.reject(&reason),
            Ok(rec) => {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                mapping::content_key(&rec).hash(&mut h);
                if !self.seen.insert(h.finish()) {
                    self.l.reject("duplicate");
                } else {
                    // Hash exactly the line written, so the provenance sha
                    // is the sha of data.jsonl.
                    let mut line = serde_json::to_string(&rec).map_err(|e| e.to_string())?;
                    line.push('\n');
                    self.hasher.update(line.as_bytes());
                    ctx.write(&rec).map_err(|e| format!("cannot write the dataset: {e}"))?;
                    self.l.accept();
                }
            }
        }
        if self.l.attempts.is_multiple_of(PUBLISH_EVERY) {
            ctx.publish(&self.l);
        }
        Ok(())
    }
}

pub async fn run(params: Params, hf: Arc<dyn HfClient>, ctx: &mut JobCtx) -> Result<Outcome, String> {
    let plan = params.plan()?;
    let mut sink = Sink {
        kind: plan.kind,
        mapping: plan.mapping.clone(),
        labels: plan.labels.clone(),
        seen: HashSet::new(),
        hasher: Sha256::new(),
        l: AcceptLoop::new(plan.max_records, 0),
    };
    ctx.set_target(plan.max_records, 0);
    let mut prov = json!({
        "kind": plan.kind.name(),
        "license": plan.license,
        "attribution": plan.attribution,
        "flags": plan.flags,
        "labelMap": plan.labels.as_ref().map(|m| m.id.clone().unwrap_or_else(|| "inline".into())),
    });
    let mut notes: Vec<String> = Vec::new();

    let local = match (&params.source, plan.catalog.map(|e| &e.source)) {
        (_, Some(EntrySource::Bundled(file))) => {
            let text = catalog::bundled_text(file).ok_or_else(|| format!("{file} is not bundled in this build"))?;
            prov["source"] = json!({"type": "bundled", "catalog": plan.catalog.map(|e| &e.id), "file": file});
            Some(read_local(file, None, text, usize::MAX)?)
        }
        (Source::File(f), _) => {
            let lr = read_local(&f.name, None, &f.content, usize::MAX)?;
            prov["source"] = json!({"type": "file", "name": f.name, "sha256": lr.sha256});
            Some(lr)
        }
        (Source::Url(u), _) => {
            let fetched = tokio::select! {
                _ = ctx.cancel.cancelled() => return Ok(Outcome::Cancelled),
                r = url_fetch::fetch(u.trim()) => r?,
            };
            let text = tabular::utf8(fetched.bytes)?;
            let lr = read_local(&fetched.final_url, fetched.content_type.as_deref(), &text, usize::MAX)?;
            prov["source"] = json!({"type": "url", "url": u.trim(), "finalUrl": fetched.final_url, "sha256": lr.sha256});
            Some(lr)
        }
        _ => None,
    };

    if let Some(lr) = local {
        prov["format"] = json!(lr.format);
        prov["sourceRows"] = json!(lr.total);
        ctx.set_provenance(prov.clone());
        sink.settle_mapping(&lr.rows)?;
        for (i, row) in lr.rows.iter().enumerate() {
            if ctx.cancel.is_cancelled() {
                return finish(ctx, sink, prov, notes, Outcome::Cancelled);
            }
            if sink.full() {
                break;
            }
            sink.push(i as u64, Some(row), ctx)?;
        }
    } else {
        // Hugging Face: a catalog preset or a dataset named by the caller.
        let (dataset, config, split, pinned) = match (&params.source, plan.catalog.map(|e| &e.source)) {
            (_, Some(EntrySource::Hf(pin))) => {
                (pin.dataset.clone(), Some(pin.config.clone()), Some(pin.split.clone()), Some(pin.revision.clone()))
            }
            (Source::Hf(r), _) => (r.dataset.trim().to_string(), r.config.clone(), r.split.clone(), None),
            _ => return Err("this import has no source".into()),
        };
        let info = tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(Outcome::Cancelled),
            r = hf.info(&dataset) => r?,
        };
        if info.gated || info.private {
            return Err(format!("{dataset} is gated or private on Hugging Face; only public datasets can be imported"));
        }
        let (config, split) = match (config, split) {
            (Some(c), Some(s)) => (c, s),
            (config, split) => {
                let splits = hf.splits(&dataset).await?;
                let pick = splits
                    .iter()
                    .find(|s| config.as_ref().is_none_or(|c| &s.config == c) && split.as_ref().is_none_or(|x| &s.split == x))
                    .ok_or_else(|| format!("{dataset} has no such config/split in the dataset viewer"))?;
                (pick.config.clone(), pick.split.clone())
            }
        };
        if let (Some(p), Some(got)) = (&pinned, &info.sha) {
            if p != got {
                notes.push(format!(
                    "Hugging Face has changed {dataset} since SenClaw checked it (pinned {}, read {}); the dataset viewer only serves the current version",
                    short(p),
                    short(got)
                ));
            }
        }
        if plan.catalog.is_none() {
            if let Some(hf_license) = &info.license {
                prov["hfLicense"] = json!(hf_license);
            }
        }
        prov["source"] = json!({
            "type": "hf", "catalog": plan.catalog.map(|e| &e.id), "dataset": dataset, "config": config, "split": split,
            "revision": info.sha, "pinnedRevision": pinned,
        });
        ctx.set_provenance(prov.clone());
        let mut offset = 0u64;
        let mut total = None;
        loop {
            if ctx.cancel.is_cancelled() {
                return finish(ctx, sink, prov, notes, Outcome::Cancelled);
            }
            if sink.full() || total.is_some_and(|t| offset >= t) {
                break;
            }
            if offset > 0 {
                tokio::select! {
                    _ = ctx.cancel.cancelled() => return finish(ctx, sink, prov, notes, Outcome::Cancelled),
                    _ = tokio::time::sleep(PAGE_PAUSE) => {}
                }
            }
            let page = tokio::select! {
                _ = ctx.cancel.cancelled() => return finish(ctx, sink, prov, notes, Outcome::Cancelled),
                r = hf.rows(&dataset, &config, &split, offset, hf::PAGE) => match r {
                    Ok(p) => p,
                    // The records so far stay; the provenance still says
                    // what was read, and the error says where it stopped.
                    Err(e) => {
                        let read = format!("{e} — stopped after {offset} rows; the {} records kept so far stay in the dataset", sink.l.accepted);
                        return finish(ctx, sink, prov, notes, Outcome::Finished).and(Err(read));
                    }
                },
            };
            if total.is_none() {
                total = Some(page.total);
                prov["sourceRows"] = json!(page.total);
                if page.partial {
                    notes.push(format!("the dataset viewer serves only the first part of {dataset}/{split}"));
                }
            }
            if page.rows.is_empty() {
                break;
            }
            if offset == 0 {
                let sample: Vec<Value> = page.rows.iter().filter_map(|(_, r)| r.clone()).collect();
                sink.settle_mapping(&sample)?;
            }
            for (idx, row) in &page.rows {
                if sink.full() {
                    break;
                }
                sink.push(*idx, row.as_ref(), ctx)?;
            }
            offset += page.rows.len() as u64;
        }
    }
    finish(ctx, sink, prov, notes, Outcome::Finished)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

fn finish(ctx: &mut JobCtx, sink: Sink, mut prov: Value, notes: Vec<String>, outcome: Outcome) -> Result<Outcome, String> {
    ctx.publish(&sink.l);
    prov["mapping"] = json!(sink.mapping);
    prov["dataSha256"] = json!(hex::encode(sink.hasher.finalize()));
    ctx.set_provenance(prov);
    if !notes.is_empty() {
        ctx.set_note(&notes.join("; "));
    }
    if outcome == Outcome::Finished && sink.l.accepted == 0 {
        return Err(format!("no row could be imported ({})", super::common::reasons(&sink.l.rejected)));
    }
    Ok(outcome)
}

/// What the import dialog shows before anything is written: the columns,
/// a few raw rows, a guessed kind and mapping, and — once a kind is chosen
/// — those rows as records, with the reason each failed one would be
/// dropped.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewRequest {
    pub source: Source,
    #[serde(default)]
    pub kind: Option<Kind>,
    #[serde(default)]
    pub mapping: BTreeMap<String, FieldSource>,
    #[serde(default)]
    pub label_map: Option<LabelMapRef>,
}

pub async fn preview(req: PreviewRequest, hf: &dyn HfClient) -> Result<Value, String> {
    let mut out = json!({});
    let rows: Vec<Value> = match &req.source {
        Source::Catalog(id) => {
            let e = catalog::entry(id).ok_or_else(|| format!("no catalog entry \"{id}\""))?;
            out["entry"] = json!(e.id);
            match &e.source {
                EntrySource::Bundled(f) => {
                    local_preview(read_local(f, None, catalog::bundled_text(f).unwrap_or_default(), PREVIEW_ROWS)?, &mut out)
                }
                EntrySource::Hf(pin) => {
                    out["config"] = json!(pin.config);
                    out["split"] = json!(pin.split);
                    hf_rows_preview(hf, &pin.dataset, &pin.config, &pin.split, &mut out).await?
                }
            }
        }
        Source::Hf(r) => {
            if !hf::valid_dataset_id(r.dataset.trim()) {
                return Err(format!("\"{}\" is not a Hugging Face dataset id (owner/name)", r.dataset));
            }
            let info = hf.info(r.dataset.trim()).await?;
            if info.gated || info.private {
                return Err(format!("{} is gated or private on Hugging Face; only public datasets can be imported", r.dataset));
            }
            out["license"] = json!(info.license);
            out["revision"] = json!(info.sha);
            hf_preview(hf, r.dataset.trim(), r.config.as_deref(), r.split.as_deref(), &mut out).await?
        }
        Source::Url(u) => {
            let f = url_fetch::fetch(u.trim()).await?;
            let text = tabular::utf8(f.bytes)?;
            local_preview(read_local(&f.final_url, f.content_type.as_deref(), &text, PREVIEW_ROWS)?, &mut out)
        }
        Source::File(f) => {
            if f.content.len() > MAX_INLINE_BYTES {
                return Err(format!("{} is larger than {} MB — give a URL instead", f.name, MAX_INLINE_BYTES / 1024 / 1024));
            }
            local_preview(read_local(&f.name, None, &f.content, PREVIEW_ROWS)?, &mut out)
        }
    };
    let order: Vec<String> = out.get("order").and_then(|o| serde_json::from_value(o.clone()).ok()).unwrap_or_default();
    if let Some(o) = out.as_object_mut() {
        o.remove("order");
    }
    let columns = tabular::ordered(tabular::columns(&rows), &order);
    let entry = match &req.source {
        Source::Catalog(id) => catalog::entry(id),
        _ => None,
    };
    let (kind, mapping, labels) = match entry {
        Some(e) => (e.kind, e.mapping.clone(), e.label_map.as_deref().and_then(catalog::label_map).cloned()),
        None => {
            let (guess_kind, guess) = mapping::suggest(&columns, req.kind);
            out["suggested"] = json!({"kind": guess_kind, "mapping": guess});
            let kind = req.kind.unwrap_or(guess_kind);
            let mapping = if req.mapping.is_empty() { guess } else { req.mapping.clone() };
            let labels = match &req.label_map {
                Some(LabelMapRef::Named(id)) => catalog::label_map(id).cloned(),
                Some(LabelMapRef::Inline(map)) => Some(LabelMap { map: map.clone(), ..LabelMap::default() }),
                None => None,
            };
            (kind, mapping, labels)
        }
    };
    out["columns"] = json!(columns);
    out["rows"] = json!(rows);
    match mapping::check_mapping(kind, &mapping) {
        Ok(()) => {
            let records: Vec<Value> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| match mapping::normalize(r, i as u64, kind, &mapping, labels.as_ref()) {
                    Ok(rec) => json!({"record": rec}),
                    Err(reason) => json!({"rejected": reason}),
                })
                .collect();
            out["records"] = json!(records);
        }
        Err(e) => out["mappingError"] = json!(e),
    }
    Ok(out)
}

fn local_preview(lr: LocalRows, out: &mut Value) -> Vec<Value> {
    out["format"] = json!(lr.format);
    out["total"] = json!(lr.total);
    if let Some(h) = lr.header {
        out["order"] = json!(h);
    }
    lr.rows
}

async fn hf_preview(
    hf: &dyn HfClient,
    dataset: &str,
    config: Option<&str>,
    split: Option<&str>,
    out: &mut Value,
) -> Result<Vec<Value>, String> {
    let splits = hf.splits(dataset).await?;
    let pick = splits
        .iter()
        .find(|s| config.is_none_or(|c| s.config == c) && split.is_none_or(|x| s.split == x))
        .ok_or_else(|| format!("{dataset} has no such config/split in the dataset viewer"))?
        .clone();
    out["splits"] = json!(splits);
    out["config"] = json!(pick.config);
    out["split"] = json!(pick.split);
    hf_rows_preview(hf, dataset, &pick.config, &pick.split, out).await
}

async fn hf_rows_preview(hf: &dyn HfClient, dataset: &str, config: &str, split: &str, out: &mut Value) -> Result<Vec<Value>, String> {
    let page = hf.rows(dataset, config, split, 0, PREVIEW_ROWS as u32).await?;
    out["total"] = json!(page.total);
    out["partial"] = json!(page.partial);
    out["order"] = json!(page.columns);
    Ok(page.rows.into_iter().filter_map(|(_, r)| r).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datagen::hf::fake::FakeHf;
    use crate::datagen::store;

    fn params(v: Value) -> Params {
        serde_json::from_value(v).unwrap()
    }

    fn records(ctx: &JobCtx) -> Vec<Value> {
        store::read_records(ctx.dir(), 0, 1000, usize::MAX).unwrap().0
    }

    #[test]
    fn sources_and_their_checks() {
        let file = |content: &str| json!({"source": {"file": {"name": "a.csv", "content": content}}, "kind": "labeled_text", "license": "private"});
        assert!(params(file("text\nxin chào\n")).validate().is_ok());
        assert!(params(file(" ")).validate().unwrap_err().contains("empty"));
        let mut no_license = file("text\nx\n");
        no_license.as_object_mut().unwrap().remove("license");
        assert!(params(no_license).validate().unwrap_err().contains("license"));
        let err = params(json!({"source": {"url": "http://localhost:18788/api/llm-config"}, "kind": "chat", "license": "x"}))
            .validate()
            .unwrap_err();
        assert!(err.contains("this machine"), "{err}");
        assert!(params(json!({"source": {"hf": {"dataset": "../etc"}}, "kind": "chat", "license": "x"})).validate().is_err());
        assert!(params(json!({"source": {"catalog": "nope"}})).validate().unwrap_err().contains("massive-vi"));
        assert!(serde_json::from_value::<Params>(json!({"source": {"ftp": "x"}})).is_err());
        assert!(serde_json::from_value::<Params>(json!({"source": {"catalog": "gsm8k"}, "maxrecords": 3})).is_err());
    }

    #[test]
    fn restricted_catalog_entries_need_consent_and_bring_their_own_mapping() {
        let err = params(json!({"source": {"catalog": "xquad-vi"}})).validate().unwrap_err();
        assert!(err.contains("acceptLicense") && err.contains("CC-BY-SA-4.0"), "{err}");
        assert!(params(json!({"source": {"catalog": "xquad-vi"}, "acceptLicense": true})).validate().is_ok());
        assert!(params(json!({"source": {"catalog": "gsm8k"}})).validate().is_ok(), "MIT needs no consent");
        let err = params(json!({"source": {"catalog": "gsm8k"}, "kind": "chat"})).validate().unwrap_err();
        assert!(err.contains("leave them out"));
    }

    #[test]
    fn inline_content_never_reaches_the_manifest() {
        let p = params(json!({"source": {"file": {"name": "a.csv", "content": "text\nbí mật\n"}}, "kind": "labeled_text", "license": "private"}));
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["source"], json!({"file": {"name": "a.csv"}}));
        assert_eq!(p.default_name(), "a");
    }

    #[tokio::test]
    async fn the_bundled_entry_imports_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let hf: Arc<dyn HfClient> = Arc::new(FakeHf::new(vec![]));
        let out = run(params(json!({"source": {"catalog": "senclaw-flows-vi"}})), hf, &mut ctx).await.unwrap();
        assert_eq!(out, Outcome::Finished);
        let s = ctx.snapshot();
        assert_eq!(s.accepted, 144);
        let prov = s.provenance.unwrap();
        assert_eq!(prov["source"]["type"], "bundled");
        assert_eq!(prov["source"]["catalog"], "senclaw-flows-vi");
        assert_eq!(prov["license"], "MIT");
        let data = std::fs::read(ctx.dir().join(store::DATA_FILE)).unwrap();
        assert_eq!(prov["dataSha256"], hex::encode(Sha256::digest(&data)), "the sha is the sha of data.jsonl");
        let recs = records(&ctx);
        assert_eq!(recs[0]["source"], json!({"row": 0}));
        assert!(recs.iter().all(|r| r["text"].is_string() && r["label"].is_string()));
    }

    #[tokio::test]
    async fn a_file_import_maps_drops_and_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let csv = "noi_dung,loai\nđơn hàng của tôi đến đâu rồi,tra_don\n,doi_tra\nđơn hàng của tôi đến đâu rồi,tra_don\ntôi muốn đổi size,doi_tra\n";
        let p = params(json!({
            "source": {"file": {"name": "khach.csv", "content": csv}},
            "kind": "labeled_text", "license": "private", "maxRecords": 10
        }));
        run(p, Arc::new(FakeHf::new(vec![])), &mut ctx).await.unwrap();
        let s = ctx.snapshot();
        assert_eq!(s.accepted, 2);
        assert_eq!(s.rejected.get("missing_text"), Some(&1));
        assert_eq!(s.rejected.get("duplicate"), Some(&1));
        let prov = s.provenance.unwrap();
        assert_eq!(prov["format"], "csv");
        assert_eq!(prov["mapping"], json!({"label": "loai", "text": "noi_dung"}), "guessed from the column names");
        assert_eq!(records(&ctx)[1], json!({"id": "4", "text": "tôi muốn đổi size", "label": "doi_tra", "source": {"row": 3}}));
    }

    #[tokio::test]
    async fn a_preset_pages_through_the_viewer_maps_labels_and_stops_at_max_records() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let mut rows = Vec::new();
        for i in 0..250 {
            let label = ["weather_query", "alarm_set", "general_joke"][i % 3];
            rows.push(json!({"id": i.to_string(), "label": label, "label_text": label, "text": format!("câu số {i}"), "lang": "vi"}));
        }
        let fake = Arc::new(FakeHf::new(rows));
        let p = params(json!({"source": {"catalog": "massive-vi"}, "acceptLicense": true, "maxRecords": 120}));
        run(p, fake.clone(), &mut ctx).await.unwrap();
        let s = ctx.snapshot();
        assert_eq!(s.accepted, 120);
        assert!(s.rejected["unmapped_label"] > 0, "alarm_set has no SenClaw skill");
        assert_eq!(*fake.calls.lock().unwrap(), vec![0, 100], "stops paging once it has enough");
        let prov = s.provenance.unwrap();
        assert_eq!(prov["source"]["dataset"], "mteb/amazon_massive_intent");
        assert_eq!(prov["source"]["revision"], "abc123");
        assert_eq!(prov["sourceRows"], 250);
        assert!(s.note.unwrap().contains("changed"), "the read commit is not the pinned one");
        let r = &records(&ctx)[0];
        assert_eq!(r["label"], "web-research");
        assert_eq!(r["sourceLabel"], "weather_query");
        assert_eq!(r["accept"], json!(["web-research", "agent-browser", "widget"]));
    }

    #[tokio::test]
    async fn gated_datasets_and_empty_results_fail_with_the_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let mut fake = FakeHf::new(vec![json!({"q": "x"})]);
        fake.info.gated = true;
        let p = params(json!({"source": {"hf": {"dataset": "org/secret"}}, "kind": "labeled_text", "license": "unknown"}));
        assert!(run(p, Arc::new(fake), &mut ctx).await.unwrap_err().contains("gated"));

        let mut ctx = JobCtx::for_test(&tmp.path().join("b"));
        let fake = FakeHf::new(vec![json!({"text": ""}), json!({"text": " "})]);
        let p = params(json!({"source": {"hf": {"dataset": "org/blank"}}, "kind": "labeled_text", "license": "mit"}));
        let err = run(p, Arc::new(fake), &mut ctx).await.unwrap_err();
        assert!(err.contains("2 missing_text"), "{err}");
    }

    #[tokio::test]
    async fn a_mapping_that_cannot_be_guessed_asks_for_one() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p = params(json!({"source": {"file": {"name": "a.jsonl", "content": "{\"foo\": 1}\n"}}, "kind": "qa_context", "license": "x"}));
        let err = run(p, Arc::new(FakeHf::new(vec![])), &mut ctx).await.unwrap_err();
        assert!(err.contains("columns: foo") && err.contains("send a mapping"), "{err}");
    }

    #[tokio::test]
    async fn preview_guesses_then_shows_records_and_rejections() {
        let fake = FakeHf::new(vec![json!({"question": "2+2?", "answer": "so 4\n#### 4"}), json!({"question": "", "answer": "x"})]);
        let out = preview(
            serde_json::from_value(json!({"source": {"hf": {"dataset": "openai/gsm8k"}}})).unwrap(),
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(out["suggested"]["kind"], "problem_answer");
        assert_eq!(out["columns"], json!(["answer", "question"]), "the fake lists columns as its rows' keys");
        assert_eq!(out["config"], "default");
        assert_eq!(out["records"][0]["record"]["problem"], "2+2?");
        assert_eq!(out["records"][1]["rejected"], "missing_problem");

        let out = preview(serde_json::from_value(json!({"source": {"catalog": "gsm8k"}})).unwrap(), &fake).await.unwrap();
        assert_eq!(out["records"][0]["record"]["answer"], "4", "the entry's own mapping, marker included");
        assert!(out.get("suggested").is_none());

        let out = preview(
            serde_json::from_value(json!({"source": {"file": {"name": "x.tsv", "content": "a\tb\n1\t2\n3\t4\n"}}, "kind": "qa_context"})).unwrap(),
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(out["total"], 2);
        assert!(out["mappingError"].as_str().unwrap().contains("question"));
    }
}
