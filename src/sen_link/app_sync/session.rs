//! One sync between this machine and a peer, for one app: compare what each
//! side holds, fetch what the other has newer, send what this side has newer.
//!
//! It runs on the machine whose person asked for it, and it is bound by the
//! policy of both: it moves a collection to this machine only if this machine
//! receives it and the peer sends it, and to the peer only if this machine
//! sends it and the peer receives it. The peer judges its own side again when
//! the calls arrive, so a bug or a lie here does not widen what it allows.
//!
//! Nothing here decides which edit wins: both apps apply the same rule (the
//! newer stamp), so a repeated or interrupted run changes nothing it should not.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::{json, Value};

use super::wire;
use super::{
    records_of, report_of, str_of, AppSync, AppSyncError, AppSyncPolicy, LocalApp, BATCH_BYTES, BATCH_RECORDS, METHOD_APPLY, METHOD_APPS, METHOD_DESCRIBE,
    METHOD_READ, METHOD_STATE,
};
use crate::sen_link::store::Peer;
use crate::sen_link::{client, LinkState};

/// A change a person has looked at and allows (only a change that was held).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct Approval {
    pub collection: String,
    pub id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RunOptions {
    /// Work out what would happen and change nothing, on either machine.
    pub dry_run: bool,
    /// Held changes this run may apply.
    pub approve: Vec<Approval>,
    /// Only these collections.
    pub collections: Option<Vec<String>>,
}

/// What the peer's error means here. `client::call` flattens a peer's error into
/// text ending in its code, so the code is read back from there.
fn peer_error(peer: &Peer, e: &anyhow::Error) -> AppSyncError {
    let text = format!("{e:#}");
    let name = &peer.name;
    if text.contains("(-32601)") {
        AppSyncError::AppUnavailable(format!("`{name}` runs a SenClaw that cannot sync apps; update it"))
    } else if text.contains("(-32010)") {
        AppSyncError::AppUnavailable(format!("`{name}` has not opened that app to this machine; its owner can allow it in Settings → SLink"))
    } else if text.contains("(-32012)") {
        AppSyncError::AppUnavailable(format!("`{name}` cannot sync that app: {text}"))
    } else if text.contains("(-32013)") {
        AppSyncError::SchemaMismatch(format!("`{name}`: {text}"))
    } else if text.contains("(-32011)") {
        AppSyncError::Busy
    } else {
        AppSyncError::Internal(text)
    }
}

/// Entries of one collection's digest as `id → (stamp, deleted)`.
fn digest_of(reply: &Value, collection: &str) -> HashMap<String, (String, bool)> {
    reply["collections"][collection]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| Some((e["id"].as_str()?.to_string(), (e["hlc"].as_str()?.to_string(), e["deleted"].as_bool().unwrap_or(false)))))
        .collect()
}

/// Ids in `from` that `to` lacks or holds older. With `equal` too, the ones
/// both hold at the same stamp (live records only): used when secrets are on at
/// both ends, because a record synced without its secrets keeps its stamp.
fn newer(from: &HashMap<String, (String, bool)>, to: &HashMap<String, (String, bool)>, equal: bool) -> Vec<String> {
    let mut ids: Vec<String> = from
        .iter()
        .filter(|(id, (hlc, deleted))| match to.get(*id) {
            None => true,
            Some((other, _)) => hlc > other || (equal && hlc == other && !deleted),
        })
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    ids
}

struct Step<'a> {
    id: &'a str,
    pull: bool,
    push: bool,
}

impl AppSync {
    /// The apps the peer has opened to this machine.
    pub async fn remote_apps(&self, st: &LinkState, peer: &Peer) -> Result<Value, AppSyncError> {
        client::call(st, peer, METHOD_APPS, json!({})).await.map_err(|e| peer_error(peer, &e))
    }

    /// Sync `app_id` with `peer`: see the module docs. Returns what was (or,
    /// for a dry run, would be) done on each side and what waits for a person.
    pub async fn run(&self, st: &LinkState, peer: &Peer, app_id: &str, opts: &RunOptions) -> Result<Value, AppSyncError> {
        let policy = peer.app_sync.get(app_id).filter(|p| p.open()).cloned().ok_or(AppSyncError::NotShared)?;
        let app = self.verified(app_id).await?;

        let remote = client::call(st, peer, METHOD_DESCRIBE, json!({ "app": app_id })).await.map_err(|e| peer_error(peer, &e))?;
        if remote["syncVersion"].as_u64() != Some(app.decl.version as u64) {
            return Err(AppSyncError::SchemaMismatch(format!("`{}` syncs `{app_id}` with contract {} and this machine with {}", peer.name, remote["syncVersion"], app.decl.version)));
        }
        let remote_policy: AppSyncPolicy = serde_json::from_value(remote["policy"].clone()).unwrap_or_default();

        // What can move, collection by collection, in dependency order.
        let wanted: Option<HashSet<&str>> = opts.collections.as_ref().map(|c| c.iter().map(String::as_str).collect());
        let mut plan = Vec::new();
        let mut steps = Vec::new();
        for c in &app.decl.collections {
            if wanted.as_ref().is_some_and(|w| !w.contains(c.id.as_str())) {
                continue;
            }
            let theirs = remote["collections"].as_array().into_iter().flatten().find(|x| x["id"] == c.id.as_str());
            let why = match theirs {
                None => Some("not_on_peer"),
                Some(t) if t["schemaVersion"].as_u64() != Some(c.schema_version as u64) => Some("schema_differs"),
                _ => None,
            };
            let pull = why.is_none() && policy.receives(&c.id) && remote_policy.sends(&c.id);
            let push = why.is_none() && policy.sends(&c.id) && remote_policy.receives(&c.id);
            let why = why.or(if pull || push {
                None
            } else if !policy.sends(&c.id) && !policy.receives(&c.id) {
                Some("off_here")
            } else {
                Some("off_on_peer")
            });
            plan.push(json!({ "collection": c.id, "pull": pull, "push": push, "why": why }));
            if pull || push {
                steps.push(Step { id: &c.id, pull, push });
            }
        }
        let mut report = json!({
            "app": app_id,
            "peer": { "id": peer.id, "name": peer.name },
            "dryRun": opts.dry_run,
            "plan": plan,
        });
        if steps.is_empty() {
            report["pull"] = report_of(opts.dry_run, vec![]);
            report["push"] = report_of(opts.dry_run, vec![]);
            report["held"] = json!(self.pending(&peer.id, app_id));
            return Ok(report);
        }

        // What each side holds.
        let involved: Vec<&str> = steps.iter().map(|s| s.id).collect();
        let local_digest = self.apps.call(&app, "POST", "/state", json!({ "collections": involved })).await?;
        let diffable: Vec<&str> = steps.iter().filter(|s| remote_policy.sends(s.id)).map(|s| s.id).collect();
        let remote_digest = if diffable.is_empty() {
            Value::Null
        } else {
            client::call(st, peer, METHOD_STATE, json!({ "app": app_id, "collections": diffable })).await.map_err(|e| peer_error(peer, &e))?
        };

        // Which ids to fetch and which to send.
        let mut pulls: Vec<(&str, Vec<String>)> = Vec::new();
        let mut pushes: Vec<(&str, Vec<String>)> = Vec::new();
        for s in &steps {
            let decl = app.decl.collection(s.id).expect("a step names a declared collection");
            let mine = digest_of(&local_digest, s.id);
            let theirs = digest_of(&remote_digest, s.id);
            // Both ends allow secrets: compare whole records, not only newer stamps.
            let full = policy.secrets && remote_policy.secrets && !decl.secret_fields.is_empty();
            if s.pull {
                let ids = newer(&theirs, &mine, full);
                if !ids.is_empty() {
                    pulls.push((s.id, ids));
                }
            }
            if s.push {
                // A peer that only receives shows no digest: send everything; the
                // other side keeps whatever it already has newer.
                let ids = if remote_policy.sends(s.id) { newer(&mine, &theirs, full) } else { newer(&mine, &HashMap::new(), false) };
                if !ids.is_empty() {
                    pushes.push((s.id, ids));
                }
            }
        }

        // Pull: read from the peer, then apply here in one go so references
        // between the records settle together.
        let approved: HashSet<(String, String)> = opts.approve.iter().map(|a| (a.collection.clone(), a.id.clone())).collect();
        let mut incoming: Vec<Value> = Vec::new();
        for (coll, ids) in &pulls {
            let decl = app.decl.collection(coll).expect("declared");
            for chunk in ids.chunks(BATCH_RECORDS) {
                let reply = client::call(st, peer, METHOD_READ, json!({ "app": app_id, "collection": coll, "ids": chunk }))
                    .await
                    .map_err(|e| peer_error(peer, &e))?;
                for mut r in records_of(&reply)? {
                    wire::strip(decl, &mut r, policy.secrets);
                    incoming.push(r);
                }
            }
        }
        let mut pulled = Vec::new();
        let mut sent_here = Vec::new();
        let (ok, needs_ok): (Vec<Value>, Vec<Value>) = incoming.into_iter().partition(|r| !approved.contains(&(str_of(r, "collection"), str_of(r, "id"))));
        for (records, risky_ok) in [(ok, false), (needs_ok, true)] {
            for batch in wire::batches(records, 1000, 1_500_000) {
                let applied = self.apply_local(&app, &policy, batch, opts.dry_run, risky_ok).await?;
                pulled.extend(applied.report["results"].as_array().cloned().unwrap_or_default());
                sent_here.extend(applied.sent);
            }
        }
        self.note_results(&peer.id, app_id, "pull", &sent_here, &Value::Array(pulled.clone()), !opts.dry_run);

        // Push: read from the app here, send to the peer.
        let mut outgoing: Vec<Value> = Vec::new();
        for (coll, ids) in &pushes {
            let decl = app.decl.collection(coll).expect("declared");
            for chunk in ids.chunks(BATCH_RECORDS) {
                let reply = self.apps.call(&app, "POST", "/read", json!({ "collection": coll, "ids": chunk, "secrets": policy.secrets })).await?;
                for mut r in records_of(&reply)? {
                    wire::strip(decl, &mut r, policy.secrets);
                    outgoing.push(r);
                }
            }
        }
        let mut pushed = Vec::new();
        for batch in wire::batches(outgoing, BATCH_RECORDS, BATCH_BYTES) {
            let reply = client::call(st, peer, METHOD_APPLY, json!({ "app": app_id, "records": batch, "dryRun": opts.dry_run }))
                .await
                .map_err(|e| peer_error(peer, &e))?;
            pushed.extend(reply["results"].as_array().cloned().unwrap_or_default());
        }

        report["pull"] = report_of(opts.dry_run, pulled);
        report["push"] = report_of(opts.dry_run, pushed);
        report["held"] = json!(self.pending(&peer.id, app_id));
        if !opts.dry_run {
            self.last.lock().unwrap().insert(
                (peer.id.clone(), app_id.to_string()),
                json!({ "at": crate::kanban::api::now(), "pull": report["pull"]["summary"], "push": report["push"]["summary"] }),
            );
        }
        Ok(report)
    }

    /// Whatever is known about one peer and app without calling anyone: what
    /// waits for a person and when the last real sync ran.
    pub fn status(&self, peer: &str, app: &str) -> Value {
        json!({ "pending": self.pending(peer, app), "last": self.last.lock().unwrap().get(&(peer.to_string(), app.to_string())) })
    }
}

/// Helpers for the management API.
pub fn policy_view(app: &LocalApp, policy: Option<&AppSyncPolicy>) -> Value {
    json!({
        "app": app.id,
        "version": app.version,
        "syncVersion": app.decl.version,
        "collections": app.decl.collections.iter().map(super::collection_json).collect::<Vec<_>>(),
        "policy": policy.cloned().unwrap_or_default(),
    })
}
