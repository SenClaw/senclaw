//! Keeping a Space App's data in step with the same app on a paired SLink peer.
//!
//! The daemon does not understand an app's data and the app does not know about
//! the network. The app declares what it holds (`sync` in its manifest,
//! [`SyncDecl`]) and serves four local calls (`/schema`, `/state`, `/read`,
//! `/apply`); this module carries those calls across the sealed channel between
//! two daemons and decides what is *allowed* to cross.
//!
//! What may cross is the operator's decision, per peer and per app
//! ([`AppSyncPolicy`]), and it takes both ends: a collection moves from A to B
//! only when A sends it **and** B receives it, each judged by that machine's own
//! policy. Nothing is open until a person opens it. Fields the app calls secret
//! or machine-local are cut here as well as by the app, from the manifest's
//! declaration, so an app that forgets cannot leak them — and an app whose live
//! schema disagrees with its declaration, or that answers its sync routes
//! without its access token, is not synced at all.
//!
//! A peer can never approve its own risky changes: `allowRisky` and
//! `adoptMatches` on a call from a peer are ignored; they come from this
//! daemon's policy and from the person using the management API.

pub mod local;
pub mod session;
pub mod wire;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::a2a;
use super::store::Peer;
use crate::apps::manifest::{SyncCollection, SyncDecl, SYNC_CONTRACT_VERSION};

/// Version of the `appsync/*` methods, advertised in the Agent Card.
pub const APP_SYNC_VERSION: u32 = 1;
pub const METHOD_APPS: &str = "appsync/apps";
pub const METHOD_DESCRIBE: &str = "appsync/describe";
pub const METHOD_STATE: &str = "appsync/state";
pub const METHOD_READ: &str = "appsync/read";
pub const METHOD_APPLY: &str = "appsync/apply";

/// Most records in one call, and most bytes: a sealed request is capped near
/// 700 KB and a reply at 5 MiB, so batches stay well inside both. Records that
/// point at each other settle best in one batch, so the count is generous.
pub const BATCH_RECORDS: usize = 500;
pub const BATCH_BYTES: usize = 300 * 1024;
/// Calls one peer may make in a minute. A sync is a dozen or so.
const CALLS_PER_MINUTE: usize = 240;
const WINDOW: Duration = Duration::from_secs(60);

// ---- policy ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Off,
    /// This machine hands the collection to the peer.
    Send,
    /// This machine takes the collection from the peer.
    Receive,
    Both,
}

impl Direction {
    pub fn sends(self) -> bool {
        matches!(self, Self::Send | Self::Both)
    }
    pub fn receives(self) -> bool {
        matches!(self, Self::Receive | Self::Both)
    }
}

/// What this machine lets one peer do with one app. Everything is off until a
/// person turns it on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSyncPolicy {
    /// Collection id → direction. Absent means off.
    pub collections: BTreeMap<String, Direction>,
    /// Let fields the app calls secret (passwords, keys) cross, in both
    /// directions. Off: they are cut from what leaves and from what arrives.
    pub secrets: bool,
    /// Fold a record that arrives under another id into the one already here
    /// when they mean the same thing (same host and user, same name).
    pub adopt_matches: bool,
}

impl AppSyncPolicy {
    pub fn open(&self) -> bool {
        self.collections.values().any(|d| *d != Direction::Off)
    }

    pub fn direction(&self, collection: &str) -> Direction {
        self.collections.get(collection).copied().unwrap_or_default()
    }

    pub fn sends(&self, collection: &str) -> bool {
        self.direction(collection).sends()
    }

    pub fn receives(&self, collection: &str) -> bool {
        self.direction(collection).receives()
    }

    /// The policy a person asked for, or why it cannot be: only collections the
    /// app declares can be switched on. `off` entries are not kept.
    pub fn validated(mut self, decl: &SyncDecl) -> Result<Self, String> {
        if let Some(bad) = self.collections.keys().find(|c| decl.collection(c).is_none()) {
            return Err(format!("the app has no sync collection `{bad}`"));
        }
        self.collections.retain(|_, d| *d != Direction::Off);
        Ok(self)
    }
}

// ---- errors ---------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub enum AppSyncError {
    /// This machine has not opened that app to this peer.
    NotShared,
    /// Too many calls in the last minute.
    Busy,
    /// The app is not installed, does not sync, is not running, or is not safe to sync.
    AppUnavailable(String),
    /// The app's live schema is not what its manifest declares.
    SchemaMismatch(String),
    Invalid(String),
    Internal(String),
}

impl AppSyncError {
    pub fn into_rpc(self) -> (i64, String) {
        match self {
            Self::NotShared => (a2a::KNOWLEDGE_NOT_SHARED, "this SenClaw has not opened that app to you".into()),
            Self::Busy => (a2a::TOO_MANY_REQUESTS, "too many sync requests; try again in a minute".into()),
            Self::AppUnavailable(m) => (a2a::APP_UNAVAILABLE, m),
            Self::SchemaMismatch(m) => (a2a::APP_SCHEMA_MISMATCH, m),
            Self::Invalid(m) => (a2a::INVALID_PARAMS, m),
            Self::Internal(m) => (a2a::INTERNAL_ERROR, m),
        }
    }
}

impl std::fmt::Display for AppSyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotShared => write!(f, "that app is not open to sync with this peer"),
            Self::Busy => write!(f, "too many sync requests"),
            Self::AppUnavailable(m) | Self::SchemaMismatch(m) | Self::Invalid(m) | Self::Internal(m) => write!(f, "{m}"),
        }
    }
}

// ---- the local app --------------------------------------------------------

/// An installed app that declares it can sync.
#[derive(Debug, Clone)]
pub struct LocalApp {
    pub id: String,
    /// The version installed from the hub; `None` for an app registered by hand.
    pub version: Option<String>,
    pub decl: SyncDecl,
}

/// How the daemon reaches the apps installed on this machine.
#[async_trait]
pub trait LocalApps: Send + Sync {
    /// The app, if it is installed and declares a valid `sync` block.
    fn lookup(&self, app: &str) -> Result<LocalApp, AppSyncError>;
    /// Every installed app that declares `sync`.
    fn syncable(&self) -> Vec<LocalApp>;
    /// One call to the app's sync routes (`path` is `/schema`, `/state`, …), made
    /// as the daemon: starting the app if it is stopped and presenting its token.
    async fn call(&self, app: &LocalApp, method: &str, path: &str, body: Value) -> Result<Value, AppSyncError>;
    /// Whether the app's own port answers a sync route without a token — an app
    /// that does is open to every process on the machine and is not synced.
    async fn answers_without_token(&self, app: &LocalApp) -> Result<bool, AppSyncError>;
}

// ---- the service ----------------------------------------------------------

/// A change that waits for a person: one record, and why.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Held {
    pub collection: String,
    pub id: String,
    pub reason: String,
    /// A name to show, taken from the record when it has one. Never a secret.
    pub name: Option<String>,
    /// `incoming` (a peer pushed it) or `pull` (found by a sync run here).
    pub from: &'static str,
}

pub struct AppSync {
    apps: Arc<dyn LocalApps>,
    calls: Mutex<HashMap<String, Vec<Instant>>>,
    /// Changes held for a person, per (peer id, app id), keyed by record.
    /// In memory: a held change is found again by the next preview.
    pending: Mutex<HashMap<(String, String), BTreeMap<(String, String), Held>>>,
    /// The last real sync per (peer id, app id), for the status view.
    pub(crate) last: Mutex<HashMap<(String, String), Value>>,
}

impl AppSync {
    pub fn new(apps: Arc<dyn LocalApps>) -> Self {
        Self { apps, calls: Mutex::new(HashMap::new()), pending: Mutex::new(HashMap::new()), last: Mutex::new(HashMap::new()) }
    }

    pub fn apps(&self) -> &dyn LocalApps {
        self.apps.as_ref()
    }

    fn allow_call(&self, peer: &str) -> Result<(), AppSyncError> {
        let mut calls = self.calls.lock().unwrap();
        let recent = calls.entry(peer.to_string()).or_default();
        let now = Instant::now();
        recent.retain(|t| now.saturating_duration_since(*t) < WINDOW);
        if recent.len() >= CALLS_PER_MINUTE {
            return Err(AppSyncError::Busy);
        }
        recent.push(now);
        Ok(())
    }

    /// The app, checked: installed, declares sync, answers, only to the daemon,
    /// and serves the schema it declared.
    pub async fn verified(&self, app_id: &str) -> Result<LocalApp, AppSyncError> {
        let app = self.apps.lookup(app_id)?;
        // First: this is also what starts a stopped app.
        let schema = self.apps.call(&app, "GET", "/schema", Value::Null).await?;
        if self.apps.answers_without_token(&app).await? {
            return Err(AppSyncError::AppUnavailable(format!(
                "`{app_id}` answers its sync routes without its access token, so any program on this machine could read it; update the app"
            )));
        }
        check_schema(&app.decl, &schema)?;
        Ok(app)
    }

    async fn open_call(&self, peer: &Peer, app_id: &str) -> Result<(LocalApp, AppSyncPolicy), AppSyncError> {
        self.allow_call(&peer.id)?;
        let policy = peer.app_sync.get(app_id).filter(|p| p.open()).cloned().ok_or(AppSyncError::NotShared)?;
        let app = self.verified(app_id).await?;
        Ok((app, policy))
    }

    /// Dispatch one `appsync/*` call from `peer`.
    pub async fn handle(&self, peer: &Peer, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        let result = match method {
            METHOD_APPS => self.apps_open_to(peer),
            METHOD_DESCRIBE => self.describe(peer, params).await,
            METHOD_STATE => self.state(peer, params).await,
            METHOD_READ => self.read(peer, params).await,
            METHOD_APPLY => self.apply(peer, params).await,
            other => return Err((a2a::METHOD_NOT_FOUND, format!("method `{other}` is not supported"))),
        };
        result.map_err(AppSyncError::into_rpc)
    }

    /// The apps this machine has opened to the caller. Says nothing about the rest.
    fn apps_open_to(&self, peer: &Peer) -> Result<Value, AppSyncError> {
        self.allow_call(&peer.id)?;
        let list: Vec<Value> = self
            .apps
            .syncable()
            .into_iter()
            .filter(|a| peer.app_sync.get(&a.id).is_some_and(AppSyncPolicy::open))
            .map(|a| json!({ "app": a.id, "version": a.version, "syncVersion": a.decl.version, "collections": a.decl.collections.iter().map(|c| &c.id).collect::<Vec<_>>() }))
            .collect();
        Ok(json!({ "apps": list }))
    }

    async fn describe(&self, peer: &Peer, params: &Value) -> Result<Value, AppSyncError> {
        let app_id = app_param(params)?;
        let (app, policy) = self.open_call(peer, app_id).await?;
        Ok(describe_json(&app, &policy))
    }

    async fn state(&self, peer: &Peer, params: &Value) -> Result<Value, AppSyncError> {
        let app_id = app_param(params)?;
        let (app, policy) = self.open_call(peer, app_id).await?;
        let allowed: Vec<String> = collections_param(&app, params)?.into_iter().filter(|c| policy.sends(c)).collect();
        if allowed.is_empty() {
            return Err(AppSyncError::NotShared);
        }
        let mut reply = self.apps.call(&app, "POST", "/state", json!({ "collections": allowed })).await?;
        // Whatever the app answered, only what was allowed leaves.
        if let Some(c) = reply.get_mut("collections").and_then(Value::as_object_mut) {
            c.retain(|k, _| allowed.contains(k));
        }
        Ok(reply)
    }

    async fn read(&self, peer: &Peer, params: &Value) -> Result<Value, AppSyncError> {
        let app_id = app_param(params)?;
        let (app, policy) = self.open_call(peer, app_id).await?;
        let collection = params["collection"].as_str().ok_or_else(|| AppSyncError::Invalid("missing params.collection".into()))?;
        let decl = app.decl.collection(collection).ok_or_else(|| AppSyncError::Invalid(format!("the app has no sync collection `{collection}`")))?;
        if !policy.sends(collection) {
            return Err(AppSyncError::NotShared);
        }
        let ids = ids_param(params)?;
        let reply = self
            .apps
            .call(&app, "POST", "/read", json!({ "collection": collection, "ids": ids, "secrets": policy.secrets }))
            .await?;
        let mut records = records_of(&reply)?;
        for r in &mut records {
            wire::strip(decl, r, policy.secrets);
        }
        Ok(json!({ "records": records }))
    }

    async fn apply(&self, peer: &Peer, params: &Value) -> Result<Value, AppSyncError> {
        let app_id = app_param(params)?;
        let (app, policy) = self.open_call(peer, app_id).await?;
        let incoming = params["records"].as_array().ok_or_else(|| AppSyncError::Invalid("missing params.records".into()))?;
        if incoming.len() > BATCH_RECORDS {
            return Err(AppSyncError::Invalid(format!("at most {BATCH_RECORDS} records per call")));
        }
        let dry_run = params["dryRun"].as_bool().unwrap_or(false);
        // The peer may not choose how risky changes are treated here: its flags
        // for that are not read.
        let outcome = self.apply_local(&app, &policy, incoming.clone(), dry_run, false).await?;
        self.note_results(&peer.id, &app.id, "incoming", &outcome.sent, &outcome.report["results"], !dry_run);
        Ok(outcome.report)
    }

    /// Apply records that came from a peer to the local app: only collections
    /// this machine receives, cut by the app's declaration, with the policy's
    /// secrets and matching choices. `risky_ok` lets held changes through.
    async fn apply_local(&self, app: &LocalApp, policy: &AppSyncPolicy, records: Vec<Value>, dry_run: bool, risky_ok: bool) -> Result<Applied, AppSyncError> {
        let mut rejected = Vec::new();
        let mut sent = Vec::new();
        for mut r in records {
            let (coll, id) = (r["collection"].as_str().unwrap_or("").to_string(), r["id"].as_str().unwrap_or("").to_string());
            match app.decl.collection(&coll) {
                Some(decl) if policy.receives(&coll) => {
                    wire::strip(decl, &mut r, policy.secrets);
                    sent.push(r);
                }
                Some(_) => rejected.push(item(&coll, &id, "rejected", "not_receiving")),
                None => rejected.push(item(&coll, &id, "rejected", "unknown_collection")),
            }
        }
        let mut results: Vec<Value> = rejected;
        if !sent.is_empty() {
            let body = json!({
                "records": sent,
                "dryRun": dry_run,
                "secrets": policy.secrets,
                "adoptMatches": policy.adopt_matches,
                "allowRisky": risky_ok,
            });
            let reply = self.apps.call(app, "POST", "/apply", body).await?;
            results.extend(reply["results"].as_array().cloned().unwrap_or_default());
        }
        Ok(Applied { report: report_of(dry_run, results), sent })
    }

    /// Remember which changes wait for a person. With `resolve`, also forget the
    /// ones that were applied; a dry run only predicts that, so it adds and never forgets.
    pub(crate) fn note_results(&self, peer: &str, app: &str, from: &'static str, sent: &[Value], results: &Value, resolve: bool) {
        let names: HashMap<(String, String), Option<String>> = sent
            .iter()
            .map(|r| ((str_of(r, "collection"), str_of(r, "id")), r["data"]["name"].as_str().map(|n| n.chars().take(80).collect())))
            .collect();
        let mut pending = self.pending.lock().unwrap();
        let held = pending.entry((peer.to_string(), app.to_string())).or_default();
        for r in results.as_array().into_iter().flatten() {
            let key = (str_of(r, "collection"), str_of(r, "id"));
            match r["status"].as_str() {
                Some("held") | Some("duplicate") => {
                    held.insert(
                        key.clone(),
                        Held {
                            collection: key.0.clone(),
                            id: key.1.clone(),
                            reason: r["reason"].as_str().unwrap_or("held").to_string(),
                            name: names.get(&key).cloned().flatten(),
                            from,
                        },
                    );
                }
                Some("rejected") | Some("error") | None => {}
                Some(_) => {
                    if resolve {
                        held.remove(&key);
                    }
                }
            }
        }
        if held.is_empty() {
            pending.remove(&(peer.to_string(), app.to_string()));
        }
    }

    /// Changes waiting for a person, for one peer and app.
    pub fn pending(&self, peer: &str, app: &str) -> Vec<Held> {
        self.pending.lock().unwrap().get(&(peer.to_string(), app.to_string())).map(|m| m.values().cloned().collect()).unwrap_or_default()
    }

    /// Forget what was held for a peer (it was removed).
    pub fn forget_peer(&self, peer: &str) {
        self.pending.lock().unwrap().retain(|(p, _), _| p != peer);
        self.last.lock().unwrap().retain(|(p, _), _| p != peer);
        self.calls.lock().unwrap().remove(peer);
    }
}

pub(crate) struct Applied {
    pub report: Value,
    /// The records that were handed to the app (after cutting), for names.
    pub sent: Vec<Value>,
}

// ---- small helpers --------------------------------------------------------

pub(crate) fn str_of(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

pub(crate) fn item(collection: &str, id: &str, status: &str, reason: &str) -> Value {
    json!({ "collection": collection, "id": id, "status": status, "reason": reason })
}

/// An apply report from result items, with the summary counted here.
pub(crate) fn report_of(dry_run: bool, results: Vec<Value>) -> Value {
    let mut summary = Map::new();
    for r in &results {
        let k = r["status"].as_str().unwrap_or("error").to_string();
        let n = summary.get(&k).and_then(Value::as_u64).unwrap_or(0) + 1;
        summary.insert(k, json!(n));
    }
    json!({ "dryRun": dry_run, "results": results, "summary": summary })
}

fn app_param(params: &Value) -> Result<&str, AppSyncError> {
    let id = params["app"].as_str().ok_or_else(|| AppSyncError::Invalid("missing params.app".into()))?;
    if valid_app_id(id) {
        Ok(id)
    } else {
        Err(AppSyncError::Invalid("params.app is not an app id".into()))
    }
}

pub fn valid_app_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn collections_param(app: &LocalApp, params: &Value) -> Result<Vec<String>, AppSyncError> {
    match params.get("collections") {
        None | Some(Value::Null) => Ok(app.decl.collections.iter().map(|c| c.id.clone()).collect()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|c| match c.as_str() {
                Some(c) if app.decl.collection(c).is_some() => Ok(c.to_string()),
                _ => Err(AppSyncError::Invalid("params.collections names a collection the app does not sync".into())),
            })
            .collect(),
        Some(_) => Err(AppSyncError::Invalid("params.collections must be a list".into())),
    }
}

fn ids_param(params: &Value) -> Result<Vec<String>, AppSyncError> {
    let ids = params["ids"].as_array().ok_or_else(|| AppSyncError::Invalid("missing params.ids".into()))?;
    if ids.len() > BATCH_RECORDS {
        return Err(AppSyncError::Invalid(format!("at most {BATCH_RECORDS} ids per call")));
    }
    ids.iter()
        .map(|i| match i.as_str() {
            Some(s) if !s.is_empty() && s.len() <= 64 => Ok(s.to_string()),
            _ => Err(AppSyncError::Invalid("params.ids holds a value that is not an id".into())),
        })
        .collect()
}

pub(crate) fn records_of(reply: &Value) -> Result<Vec<Value>, AppSyncError> {
    reply["records"].as_array().cloned().ok_or_else(|| AppSyncError::Internal("the app's read answer has no records".into()))
}

fn set_of(v: &Value, key: &str) -> Vec<String> {
    let mut s: Vec<String> = v[key].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
    s.sort();
    s
}

fn sorted(v: &[String]) -> Vec<String> {
    let mut s = v.to_vec();
    s.sort();
    s
}

/// The app serves the schema its manifest declared. A field the app now treats
/// as secret that the manifest does not list would not be cut here.
pub(crate) fn check_schema(decl: &SyncDecl, live: &Value) -> Result<(), AppSyncError> {
    let mismatch = |m: String| Err(AppSyncError::SchemaMismatch(m));
    if live["syncVersion"].as_u64() != Some(SYNC_CONTRACT_VERSION as u64) {
        return mismatch(format!("the app speaks sync version {} and this SenClaw {}; update whichever is older", live["syncVersion"], SYNC_CONTRACT_VERSION));
    }
    for c in &decl.collections {
        let Some(l) = live["collections"].as_array().into_iter().flatten().find(|x| x["id"] == c.id.as_str()) else {
            return mismatch(format!("the app does not serve the sync collection `{}` its manifest declares", c.id));
        };
        if l["schemaVersion"].as_u64() != Some(c.schema_version as u64) {
            return mismatch(format!("collection `{}`: the app serves schema {} but its manifest declares {}", c.id, l["schemaVersion"], c.schema_version));
        }
        if set_of(l, "secretFields") != sorted(&c.secret_fields) || set_of(l, "localFields") != sorted(&c.local_fields) {
            return mismatch(format!("collection `{}`: the app's secret or machine-local fields differ from its manifest", c.id));
        }
    }
    Ok(())
}

pub(crate) fn describe_json(app: &LocalApp, policy: &AppSyncPolicy) -> Value {
    json!({
        "app": app.id,
        "version": app.version,
        "syncVersion": app.decl.version,
        "collections": app.decl.collections.iter().map(collection_json).collect::<Vec<_>>(),
        "policy": policy,
    })
}

pub(crate) fn collection_json(c: &SyncCollection) -> Value {
    json!({
        "id": c.id,
        "schemaVersion": c.schema_version,
        "secretFields": c.secret_fields,
        "localFields": c.local_fields,
        "matchKey": c.match_key,
        "guardFields": c.guard_fields,
        "refs": c.refs.iter().map(|(f, t)| json!({ "field": f, "target": t })).collect::<Vec<_>>(),
    })
}
