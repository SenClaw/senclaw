//! Two real daemons (SLink listener, pairing, sealed channel) with a stand-in
//! app on each side. The stand-in is deliberately careless — it hands out its
//! secret and machine-local fields whatever it is asked — so what these tests
//! show is what the daemon enforces, not what a well-behaved app would do.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use tower::ServiceExt;

use super::session::{Approval, RunOptions};
use super::*;
use crate::sen_link::store::Peer;
use crate::sen_link::testkit::{node, node_with, paired, Node};
use crate::sen_link::{api_router, client};

// ---- the stand-in app -------------------------------------------------------

fn manifest() -> Value {
    json!({ "hub": { "version": "1.3.0" }, "sync": {
        "version": 1, "endpoint": "/api/sync", "order": ["notes", "tags"],
        "collections": [
            { "id": "notes", "schemaVersion": 1, "secretFields": ["secret"], "localFields": ["pinned"], "matchKey": ["title"], "guardFields": [], "refs": [] },
            { "id": "tags", "schemaVersion": 1, "secretFields": [], "localFields": [], "matchKey": [], "guardFields": [], "refs": [] }
        ] } })
}

fn live_schema() -> Value {
    json!({ "syncVersion": 1, "collections": [
        { "id": "notes", "schemaVersion": 1, "secretFields": ["secret"], "localFields": ["pinned"] },
        { "id": "tags", "schemaVersion": 1, "secretFields": [], "localFields": [] } ] })
}

#[derive(Clone)]
struct Rec {
    hlc: String,
    deleted: bool,
    data: Value,
}

#[derive(Default)]
struct FakeApp {
    installed: bool,
    unguarded: bool,
    schema: Option<Value>,
    store: Mutex<BTreeMap<String, BTreeMap<String, Rec>>>,
    /// Every apply body the app was handed.
    applied: Mutex<Vec<Value>>,
    calls: Mutex<Vec<String>>,
}

impl FakeApp {
    fn new() -> Arc<Self> {
        Arc::new(Self { installed: true, ..Default::default() })
    }

    fn with(f: impl FnOnce(&mut FakeApp)) -> Arc<Self> {
        let mut a = Self { installed: true, ..Default::default() };
        f(&mut a);
        Arc::new(a)
    }

    fn put(&self, coll: &str, id: &str, hlc: &str, data: Value) {
        self.store.lock().unwrap().entry(coll.into()).or_default().insert(id.into(), Rec { hlc: hlc.into(), deleted: false, data });
    }

    fn get(&self, coll: &str, id: &str) -> Option<Value> {
        self.store.lock().unwrap().get(coll)?.get(id).filter(|r| !r.deleted).map(|r| r.data.clone())
    }

    fn applied_bodies(&self) -> Vec<Value> {
        self.applied.lock().unwrap().clone()
    }
}

#[async_trait]
impl LocalApps for FakeApp {
    fn lookup(&self, app: &str) -> Result<LocalApp, AppSyncError> {
        if !self.installed || app != "notes-app" {
            return Err(AppSyncError::AppUnavailable(format!("`{app}` is not installed on this machine")));
        }
        Ok(LocalApp { id: app.into(), version: Some("1.3.0".into()), decl: SyncDecl::parse(&manifest()).unwrap().unwrap() })
    }

    fn syncable(&self) -> Vec<LocalApp> {
        self.lookup("notes-app").into_iter().collect()
    }

    async fn call(&self, _app: &LocalApp, method: &str, path: &str, body: Value) -> Result<Value, AppSyncError> {
        self.calls.lock().unwrap().push(format!("{method} {path}"));
        let mut store = self.store.lock().unwrap();
        match path {
            "/schema" => Ok(self.schema.clone().unwrap_or_else(live_schema)),
            // Careless on purpose: every collection, not only the asked ones.
            "/state" => {
                let out: serde_json::Map<String, Value> = ["notes", "tags"]
                    .iter()
                    .map(|c| {
                        let list: Vec<Value> = store.get(*c).into_iter().flatten().map(|(id, r)| json!({ "id": id, "hlc": r.hlc, "deleted": r.deleted })).collect();
                        (c.to_string(), json!(list))
                    })
                    .collect();
                Ok(json!({ "collections": out }))
            }
            // Careless on purpose: secret and local fields included, whatever `secrets` says.
            "/read" => {
                let coll = body["collection"].as_str().unwrap_or("");
                let records: Vec<Value> = body["ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|i| i.as_str())
                    .filter_map(|id| store.get(coll)?.get(id).map(|r| (id, r.clone())))
                    .map(|(id, r)| json!({ "collection": coll, "id": id, "hlc": r.hlc, "deleted": r.deleted, "data": r.data }))
                    .collect();
                Ok(json!({ "records": records }))
            }
            "/apply" => {
                self.applied.lock().unwrap().push(body.clone());
                let dry = body["dryRun"].as_bool().unwrap_or(false);
                let risky_ok = body["allowRisky"].as_bool().unwrap_or(false);
                let mut results = Vec::new();
                for r in body["records"].as_array().into_iter().flatten() {
                    let (coll, id, hlc) = (r["collection"].as_str().unwrap_or(""), r["id"].as_str().unwrap_or(""), r["hlc"].as_str().unwrap_or(""));
                    let existing = store.get(coll).and_then(|m| m.get(id)).cloned();
                    let status = if existing.as_ref().is_some_and(|e| e.hlc.as_str() >= hlc) {
                        "skipped"
                    } else if r["data"]["risky"] == true && !risky_ok {
                        "held"
                    } else {
                        if !dry {
                            store.entry(coll.into()).or_default().insert(id.into(), Rec { hlc: hlc.into(), deleted: r["deleted"] == true, data: r["data"].clone() });
                        }
                        if existing.is_some() { "updated" } else { "added" }
                    };
                    results.push(json!({ "collection": coll, "id": id, "status": status, "reason": if status == "held" { json!("risky") } else { Value::Null } }));
                }
                Ok(json!({ "dryRun": dry, "results": results }))
            }
            _ => Err(AppSyncError::Internal("no such route".into())),
        }
    }

    async fn answers_without_token(&self, _app: &LocalApp) -> Result<bool, AppSyncError> {
        Ok(self.unguarded)
    }
}

// ---- two daemons --------------------------------------------------------------

struct Pair {
    a: Node,
    b: Node,
    at_a: Peer,
    at_b: Peer,
    app_a: Arc<FakeApp>,
    app_b: Arc<FakeApp>,
}

async fn pair_with(app_a: Arc<FakeApp>, app_b: Arc<FakeApp>) -> Pair {
    let (ra, rb) = (app_a.clone(), app_b.clone());
    let a = node_with("alpha", json!({}), move |st| st.with_app_sync(ra)).await;
    let b = node_with("beta", json!({}), move |st| st.with_app_sync(rb)).await;
    let (at_a, at_b) = paired(&a, &b).await;
    Pair { a, b, at_a, at_b, app_a, app_b }
}

async fn pair() -> Pair {
    pair_with(FakeApp::new(), FakeApp::new()).await
}

fn policy(notes: Direction, secrets: bool) -> AppSyncPolicy {
    AppSyncPolicy { collections: BTreeMap::from([("notes".to_string(), notes)]), secrets, adopt_matches: false }
}

impl Pair {
    /// What alpha lets beta do, and what beta lets alpha do.
    fn allow(&self, alpha_to_beta: AppSyncPolicy, beta_to_alpha: AppSyncPolicy) {
        self.a.st.store.update(&self.at_a.id, |p| drop(p.app_sync.insert("notes-app".into(), alpha_to_beta))).unwrap();
        self.b.st.store.update(&self.at_b.id, |p| drop(p.app_sync.insert("notes-app".into(), beta_to_alpha))).unwrap();
    }

    fn alpha(&self) -> Peer {
        self.a.st.store.get(&self.at_a.id).unwrap()
    }

    async fn run_alpha(&self, opts: RunOptions) -> Result<Value, AppSyncError> {
        self.a.st.app_sync.as_ref().unwrap().run(&self.a.st, &self.alpha(), "notes-app", &opts).await
    }

    async fn from_alpha(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        client::call(&self.a.st, &self.alpha(), method, params).await
    }
}

fn note(title: &str, extra: Value) -> Value {
    let mut v = json!({ "title": title, "secret": "s3cret", "pinned": true });
    for (k, x) in extra.as_object().into_iter().flatten() {
        v[k] = x.clone();
    }
    v
}

const T1: &str = "0000000001000-00000-aaaa";
const T2: &str = "0000000002000-00000-bbbb";

// ---- tests --------------------------------------------------------------------

#[tokio::test]
async fn nothing_is_open_until_a_person_opens_it() {
    let p = pair().await;
    p.app_b.put("notes", "n1", T1, note("hello", json!({})));
    for (method, params) in [
        (METHOD_DESCRIBE, json!({ "app": "notes-app" })),
        (METHOD_STATE, json!({ "app": "notes-app" })),
        (METHOD_READ, json!({ "app": "notes-app", "collection": "notes", "ids": ["n1"] })),
        (METHOD_APPLY, json!({ "app": "notes-app", "records": [] })),
    ] {
        let e = p.from_alpha(method, params).await.unwrap_err().to_string();
        assert!(e.contains("(-32010)"), "{method}: {e}");
    }
    // It does not even say which apps it has.
    assert_eq!(p.from_alpha(METHOD_APPS, json!({})).await.unwrap()["apps"], json!([]));
    assert!(p.app_b.calls.lock().unwrap().is_empty(), "the app was not so much as asked");
    // Opened to alpha, it shows up.
    p.allow(policy(Direction::Off, false), policy(Direction::Send, false));
    let apps = p.from_alpha(METHOD_APPS, json!({})).await.unwrap();
    assert_eq!(apps["apps"][0]["app"], "notes-app");
    assert_eq!(apps["apps"][0]["version"], "1.3.0");
}

#[tokio::test]
async fn a_collection_moves_only_when_one_side_sends_and_the_other_receives() {
    let p = pair().await;
    p.app_a.put("notes", "from-a", T1, note("on alpha", json!({})));
    p.app_b.put("notes", "from-b", T1, note("on beta", json!({})));

    // Alpha takes and gives; beta only gives.
    p.allow(policy(Direction::Both, false), policy(Direction::Send, false));
    let r = p.run_alpha(RunOptions::default()).await.unwrap();
    assert_eq!(r["plan"][0]["pull"], true);
    assert_eq!(r["plan"][0]["push"], false);
    assert!(p.app_a.get("notes", "from-b").is_some(), "alpha pulled what beta sends");
    assert!(p.app_b.get("notes", "from-a").is_none(), "beta does not receive, so nothing went");
    assert!(p.app_b.applied_bodies().is_empty());

    // A push attempted by hand is turned away by beta itself.
    let rec = json!({ "collection": "notes", "id": "from-a", "hlc": T2, "deleted": false, "data": { "title": "x" } });
    let reply = p.from_alpha(METHOD_APPLY, json!({ "app": "notes-app", "records": [rec] })).await.unwrap();
    assert_eq!(reply["results"][0]["status"], "rejected");
    assert_eq!(reply["results"][0]["reason"], "not_receiving");
    assert!(p.app_b.applied_bodies().is_empty(), "the app never saw it");

    // And a collection that is off cannot be read either.
    let e = p.from_alpha(METHOD_READ, json!({ "app": "notes-app", "collection": "tags", "ids": ["x"] })).await.unwrap_err().to_string();
    assert!(e.contains("(-32010)"), "{e}");
}

#[tokio::test]
async fn the_state_a_peer_sees_is_only_what_is_sent_to_it() {
    let p = pair().await;
    p.app_b.put("notes", "n1", T1, note("hello", json!({})));
    p.app_b.put("tags", "t1", T1, json!({ "name": "private" }));
    p.allow(policy(Direction::Off, false), policy(Direction::Send, false));
    let state = p.from_alpha(METHOD_STATE, json!({ "app": "notes-app" })).await.unwrap();
    assert!(state["collections"]["notes"].is_array());
    assert!(state["collections"].get("tags").is_none(), "the app answered with tags too; they did not leave: {state}");
}

#[tokio::test]
async fn secrets_and_machine_local_fields_cross_only_when_both_ends_allow() {
    // Whatever the app hands over, the daemon cuts it.
    for (alpha_secrets, beta_secrets, crosses) in [(false, false, false), (true, false, false), (false, true, false), (true, true, true)] {
        let p = pair().await;
        p.app_a.put("notes", "n1", T1, note("hello", json!({})));
        p.allow(policy(Direction::Send, alpha_secrets), policy(Direction::Receive, beta_secrets));
        let r = p.run_alpha(RunOptions::default()).await.unwrap();
        assert_eq!(r["push"]["summary"]["added"], 1, "{r}");
        let stored = p.app_b.get("notes", "n1").unwrap();
        assert_eq!(stored.get("secret").is_some(), crosses, "alpha={alpha_secrets} beta={beta_secrets}: {stored}");
        assert!(stored.get("pinned").is_none(), "a machine-local field never crosses: {stored}");
        assert_eq!(stored["title"], "hello");
    }
}

#[tokio::test]
async fn a_peer_cannot_approve_its_own_risky_changes_and_this_machine_can() {
    let p = pair().await;
    p.app_a.put("notes", "n1", T1, note("odd", json!({ "risky": true })));
    p.allow(policy(Direction::Send, false), policy(Direction::Receive, false));

    // Alpha pushes, claiming the change is approved. Beta ignores the claim.
    let rec = json!({ "collection": "notes", "id": "n1", "hlc": T1, "deleted": false, "data": { "title": "odd", "risky": true } });
    let reply = p
        .from_alpha(METHOD_APPLY, json!({ "app": "notes-app", "records": [rec], "allowRisky": true, "adoptMatches": true }))
        .await
        .unwrap();
    assert_eq!(reply["results"][0]["status"], "held");
    let sent = p.app_b.applied_bodies();
    assert_eq!(sent[0]["allowRisky"], false);
    assert_eq!(sent[0]["adoptMatches"], false, "folding records is beta's own setting, not alpha's call");
    assert!(p.app_b.get("notes", "n1").is_none());
    let waiting = p.b.st.app_sync.as_ref().unwrap().pending(&p.at_b.id, "notes-app");
    assert_eq!((waiting[0].id.as_str(), waiting[0].reason.as_str(), waiting[0].from), ("n1", "risky", "incoming"));
    assert_eq!(waiting[0].name, None, "the record had no name field");

    // Beta's person looks at it and approves; beta pulls it and applies it with the flag.
    p.allow(policy(Direction::Send, false), policy(Direction::Both, false));
    let beta_view = p.b.st.store.get(&p.at_b.id).unwrap();
    let opts = RunOptions { approve: vec![Approval { collection: "notes".into(), id: "n1".into() }], ..Default::default() };
    let run = p.b.st.app_sync.as_ref().unwrap().run(&p.b.st, &beta_view, "notes-app", &opts).await.unwrap();
    assert_eq!(run["pull"]["summary"]["added"], 1, "{run}");
    assert!(p.app_b.get("notes", "n1").is_some());
    assert!(p.b.st.app_sync.as_ref().unwrap().pending(&p.at_b.id, "notes-app").is_empty(), "nothing left waiting");
    assert_eq!(p.app_b.applied_bodies().last().unwrap()["allowRisky"], true);
}

#[tokio::test]
async fn a_preview_adds_held_changes_to_the_list_but_never_clears_it() {
    let p = pair().await;
    p.app_a.put("notes", "n1", T1, note("odd", json!({ "risky": true })));
    p.allow(policy(Direction::Send, false), policy(Direction::Both, false));
    let beta = p.b.st.store.get(&p.at_b.id).unwrap();
    let svc = p.b.st.app_sync.as_ref().unwrap();
    let dry = RunOptions { dry_run: true, ..Default::default() };
    let r = svc.run(&p.b.st, &beta, "notes-app", &dry).await.unwrap();
    assert_eq!(r["held"][0]["id"], "n1", "a preview lists what would wait");
    // A preview that is told the change is approved only predicts success: it must not drop the entry.
    let approve = RunOptions { dry_run: true, approve: vec![Approval { collection: "notes".into(), id: "n1".into() }], ..Default::default() };
    let r = svc.run(&p.b.st, &beta, "notes-app", &approve).await.unwrap();
    assert_eq!(r["pull"]["summary"]["added"], 1);
    assert_eq!(svc.pending(&p.at_b.id, "notes-app").len(), 1, "still waiting: nothing was applied");
    assert!(p.app_b.get("notes", "n1").is_none());
}

#[tokio::test]
async fn a_dry_run_changes_nothing_on_either_side() {
    let p = pair().await;
    p.app_a.put("notes", "a1", T1, note("alpha note", json!({})));
    p.app_b.put("notes", "b1", T1, note("beta note", json!({})));
    p.allow(policy(Direction::Both, false), policy(Direction::Both, false));
    let r = p.run_alpha(RunOptions { dry_run: true, ..Default::default() }).await.unwrap();
    assert_eq!((r["pull"]["summary"]["added"].as_u64(), r["push"]["summary"]["added"].as_u64()), (Some(1), Some(1)), "{r}");
    assert!(p.app_a.get("notes", "b1").is_none() && p.app_b.get("notes", "a1").is_none());
    assert!(p.alpha_last().is_none(), "a preview is not a sync");

    let r = p.run_alpha(RunOptions::default()).await.unwrap();
    assert_eq!(r["pull"]["summary"]["added"], 1);
    assert!(p.app_a.get("notes", "b1").is_some() && p.app_b.get("notes", "a1").is_some());
    assert!(p.alpha_last().is_some());

    // Nothing is newer on either side now: the second run moves nothing.
    let again = p.run_alpha(RunOptions::default()).await.unwrap();
    assert_eq!(again["pull"]["results"], json!([]));
    assert_eq!(again["push"]["results"], json!([]));
}

impl Pair {
    /// When alpha last really synced, if it has.
    fn alpha_last(&self) -> Option<Value> {
        let last = self.a.st.app_sync.as_ref().unwrap().status(&self.at_a.id, "notes-app")["last"].clone();
        (!last.is_null()).then_some(last)
    }
}

#[tokio::test]
async fn with_secrets_on_at_both_ends_whole_records_are_compared_again() {
    let p = pair().await;
    p.app_a.put("notes", "n1", T1, note("hello", json!({})));
    // Beta also sends, so alpha can see its digest and send only what is newer.
    p.allow(policy(Direction::Send, false), policy(Direction::Both, false));
    p.run_alpha(RunOptions::default()).await.unwrap();
    assert_eq!(p.app_b.applied_bodies().len(), 1);

    // Same stamp on both sides: with secrets off there is nothing to send.
    p.run_alpha(RunOptions::default()).await.unwrap();
    assert_eq!(p.app_b.applied_bodies().len(), 1, "nothing newer, nothing sent");

    // Secrets on at both ends: the record goes again, so the app can fill the secret in.
    p.allow(policy(Direction::Send, true), policy(Direction::Both, true));
    p.run_alpha(RunOptions::default()).await.unwrap();
    let bodies = p.app_b.applied_bodies();
    assert_eq!(bodies.len(), 2, "the equal-stamp record was sent again");
    assert_eq!(bodies[1]["records"][0]["data"]["secret"], "s3cret");
    assert_eq!(bodies[1]["secrets"], true);
}

#[tokio::test]
async fn a_peer_that_only_receives_gets_everything_each_time_and_keeps_what_it_has_newer() {
    let p = pair().await;
    p.app_a.put("notes", "n1", T1, note("hello", json!({})));
    p.allow(policy(Direction::Send, false), policy(Direction::Receive, false));
    p.run_alpha(RunOptions::default()).await.unwrap();
    p.run_alpha(RunOptions::default()).await.unwrap();
    // It shows no digest, so alpha cannot tell what beta has: it sends again, and beta's app says "same".
    let bodies = p.app_b.applied_bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1]["records"][0]["id"], "n1");
}

#[tokio::test]
async fn an_app_that_is_unsafe_or_wrong_is_not_synced() {
    // Answers its sync routes to anyone on the machine.
    let open = FakeApp::with(|a| a.unguarded = true);
    let p = pair_with(FakeApp::new(), open).await;
    p.allow(policy(Direction::Both, false), policy(Direction::Both, false));
    let e = p.from_alpha(METHOD_DESCRIBE, json!({ "app": "notes-app" })).await.unwrap_err().to_string();
    assert!(e.contains("(-32012)") && e.contains("access token"), "{e}");

    // Serves a secret field its manifest does not list.
    let drifted = FakeApp::with(|a| {
        let mut s = live_schema();
        s["collections"][0]["secretFields"] = json!(["secret", "token"]);
        a.schema = Some(s);
    });
    let p = pair_with(FakeApp::new(), drifted).await;
    p.allow(policy(Direction::Both, false), policy(Direction::Both, false));
    let e = p.from_alpha(METHOD_STATE, json!({ "app": "notes-app" })).await.unwrap_err().to_string();
    assert!(e.contains("(-32013)"), "{e}");

    // Not installed.
    let none = FakeApp::with(|a| a.installed = false);
    let p = pair_with(FakeApp::new(), none).await;
    p.allow(policy(Direction::Both, false), policy(Direction::Both, false));
    let e = p.from_alpha(METHOD_DESCRIBE, json!({ "app": "notes-app" })).await.unwrap_err().to_string();
    assert!(e.contains("(-32012)") && e.contains("not installed"), "{e}");
    // ... and the run on the other machine says so rather than failing obscurely.
    let err = p.run_alpha(RunOptions::default()).await.unwrap_err();
    assert!(matches!(err, AppSyncError::AppUnavailable(ref m) if m.contains("not installed")), "{err:?}");
}

#[tokio::test]
async fn a_daemon_without_app_sync_answers_method_not_found() {
    let a = node_with("alpha", json!({}), |st| st.with_app_sync(FakeApp::new())).await;
    let b = node("beta", json!({})).await;
    let (at_a, _) = paired(&a, &b).await;
    a.st.store.update(&at_a.id, |p| drop(p.app_sync.insert("notes-app".into(), policy(Direction::Both, false)))).unwrap();
    let peer = a.st.store.get(&at_a.id).unwrap();
    let err = a.st.app_sync.as_ref().unwrap().run(&a.st, &peer, "notes-app", &RunOptions::default()).await.unwrap_err();
    assert!(matches!(err, AppSyncError::AppUnavailable(ref m) if m.contains("cannot sync apps")), "{err:?}");
}

#[tokio::test]
async fn a_peer_cannot_flood_the_app() {
    let st = AppSync::new(FakeApp::new());
    let mut peer = pair().await.at_a;
    peer.app_sync.insert("notes-app".into(), policy(Direction::Both, false));
    for _ in 0..CALLS_PER_MINUTE {
        assert!(st.handle(&peer, METHOD_APPS, &json!({})).await.is_ok());
    }
    assert_eq!(st.handle(&peer, METHOD_APPS, &json!({})).await.unwrap_err().0, a2a::TOO_MANY_REQUESTS);
    st.forget_peer(&peer.id);
    assert!(st.handle(&peer, METHOD_APPS, &json!({})).await.is_ok(), "forgetting a peer clears its count");
}

#[tokio::test]
async fn unknown_methods_collections_and_garbage_params_are_refused_cleanly() {
    let p = pair().await;
    p.allow(policy(Direction::Both, false), policy(Direction::Both, false));
    let e = p.from_alpha("appsync/nope", json!({})).await.unwrap_err().to_string();
    assert!(e.contains("(-32601)"), "{e}");
    for (method, params, code) in [
        (METHOD_STATE, json!({}), "-32602"),
        (METHOD_STATE, json!({ "app": "../x" }), "-32602"),
        (METHOD_STATE, json!({ "app": "notes-app", "collections": ["settings"] }), "-32602"),
        (METHOD_READ, json!({ "app": "notes-app", "collection": "settings", "ids": [] }), "-32602"),
        (METHOD_READ, json!({ "app": "notes-app", "collection": "notes", "ids": [1] }), "-32602"),
        (METHOD_APPLY, json!({ "app": "notes-app" }), "-32602"),
    ] {
        let e = p.from_alpha(method, params.clone()).await.unwrap_err().to_string();
        assert!(e.contains(&format!("({code})")), "{method} {params}: {e}");
    }
    let too_many: Vec<Value> = (0..=BATCH_RECORDS).map(|i| json!({ "collection": "notes", "id": format!("n{i}"), "hlc": T1, "data": {} })).collect();
    let e = p.from_alpha(METHOD_APPLY, json!({ "app": "notes-app", "records": too_many })).await.unwrap_err().to_string();
    assert!(e.contains("(-32602)"), "{e}");
}

#[test]
fn a_policy_names_only_collections_the_app_declares_and_keeps_only_what_is_on() {
    let decl = SyncDecl::parse(&manifest()).unwrap().unwrap();
    let ok = AppSyncPolicy { collections: BTreeMap::from([("notes".into(), Direction::Both), ("tags".into(), Direction::Off)]), ..Default::default() }.validated(&decl).unwrap();
    assert_eq!(ok.collections.keys().collect::<Vec<_>>(), ["notes"]);
    assert!(ok.sends("notes") && ok.receives("notes") && !ok.sends("tags"));
    let bad = AppSyncPolicy { collections: BTreeMap::from([("settings".into(), Direction::Both)]), ..Default::default() }.validated(&decl);
    assert!(bad.unwrap_err().contains("settings"));
    assert!(!AppSyncPolicy::default().open());
    // How a policy is written on disk and in the API.
    let back: AppSyncPolicy = serde_json::from_value(json!({ "collections": { "notes": "send" }, "secrets": true })).unwrap();
    assert!(back.sends("notes") && !back.receives("notes") && back.secrets && !back.adopt_matches);
    assert_eq!(serde_json::to_value(&back).unwrap()["collections"]["notes"], "send");
}

// ---- the management API ----------------------------------------------------------

async fn call(p: &Pair, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut req = Request::builder().method(method).uri(path);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = api_router(Arc::clone(&p.a.st)).oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn the_management_api_sets_policy_previews_and_runs() {
    let p = pair().await;
    p.app_a.put("notes", "a1", T1, note("alpha note", json!({})));
    p.app_b.put("notes", "b1", T1, note("beta note", json!({})));
    let id = &p.at_a.id;
    let base = format!("/api/sen-link/peers/{id}/app-sync");

    // Beta opens its side; alpha is asked what it may do.
    p.b.st.store.update(&p.at_b.id, |q| drop(q.app_sync.insert("notes-app".into(), policy(Direction::Both, false)))).unwrap();
    let (s, list) = call(&p, "GET", &base, None).await;
    assert_eq!(s, 200);
    assert_eq!(list["apps"][0]["app"], "notes-app");
    assert_eq!(list["apps"][0]["policy"]["collections"], json!({}), "closed by default");
    assert_eq!(list["remote"]["apps"][0]["app"], "notes-app", "and what beta has opened shows beside it");

    // Unknown collection, then secrets without confirmation, then a good one.
    let put_url = format!("{base}/notes-app");
    let put = |body: Value| {
        let (p, url) = (&p, put_url.clone());
        async move { call(p, "PUT", &url, Some(body)).await }
    };
    assert_eq!(put(json!({ "policy": { "collections": { "settings": "both" } } })).await.0, 400);
    let (s, e) = put(json!({ "policy": { "collections": { "notes": "both" }, "secrets": true } })).await;
    assert_eq!((s, e["error"].as_str().unwrap().contains("confirmSecrets")), (400, true));
    let (s, v) = put(json!({ "policy": { "collections": { "notes": "both", "tags": "off" } } })).await;
    assert_eq!(s, 200);
    assert_eq!(v["policy"]["collections"], json!({ "notes": "both" }));
    assert_eq!(p.alpha().app_sync["notes-app"].direction("notes"), Direction::Both);
    assert_eq!(put(json!({ "policy": { "collections": { "notes": "both" }, "secrets": true }, "confirmSecrets": true })).await.0, 200);
    assert!(p.alpha().app_sync["notes-app"].secrets);
    // Turning it back off needs no confirmation, and an empty policy closes the app.
    assert_eq!(put(json!({ "policy": { "collections": { "notes": "both" } } })).await.0, 200);
    assert!(!p.alpha().app_sync["notes-app"].secrets);

    let (s, preview) = call(&p, "POST", &format!("{base}/notes-app/preview"), None).await;
    assert_eq!(s, 200, "{preview}");
    assert_eq!(preview["dryRun"], true);
    assert!(p.app_a.get("notes", "b1").is_none());
    let (s, run) = call(&p, "POST", &format!("{base}/notes-app/run"), Some(json!({}))).await;
    assert_eq!(s, 200, "{run}");
    assert!(p.app_a.get("notes", "b1").is_some() && p.app_b.get("notes", "a1").is_some());
    let (_, one) = call(&p, "GET", &format!("{base}/notes-app"), None).await;
    assert!(one["status"]["last"]["at"].is_number());

    let (s, closed) = put(json!({ "policy": {} })).await;
    assert_eq!(s, 200);
    assert_eq!(closed["policy"]["collections"], json!({}));
    assert!(p.alpha().app_sync.is_empty());
    let (s, e) = call(&p, "POST", &format!("{base}/notes-app/run"), None).await;
    assert_eq!((s, e["code"].as_str()), (409, Some("not_open")));
    assert_eq!(call(&p, "GET", "/api/sen-link/peers/nope/app-sync", None).await.0, 404);
}

#[tokio::test]
async fn pairing_again_keeps_what_was_opened() {
    let p = pair().await;
    p.allow(policy(Direction::Both, true), policy(Direction::Both, false));
    // The same daemon pairing again replaces its record and keeps the choices made about it.
    let (code, _) = p.b.st.store.create_invite(crate::kanban::api::now()).unwrap();
    let body = json!({ "addr": format!("127.0.0.1:{}", p.b.port), "code": code });
    let (s, _) = call(&p, "POST", "/api/sen-link/peers/pair", Some(body)).await;
    assert_eq!(s, 200);
    assert_eq!(p.a.st.store.by_daemon_id(p.b.st.store.daemon_id()).unwrap().app_sync["notes-app"], policy(Direction::Both, true));
}
