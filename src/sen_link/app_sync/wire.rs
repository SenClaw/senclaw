//! What a record looks like when it crosses between two machines, and the one
//! place that cuts it down to what is allowed to.
//!
//! The app cuts these fields too. Cutting here as well, from the manifest's
//! declaration, means an app that forgets — or a peer that sends a field it
//! should not — does not move a password or a machine's own state.

use serde_json::{json, Value};

use crate::apps::manifest::SyncCollection;

/// Keep only the five fields a record has on the wire, and take out of its
/// data the fields that stay on a machine (always) and the secret ones (unless
/// `keep_secrets`). Applied to what leaves this machine and to what arrives.
pub fn strip(decl: &SyncCollection, record: &mut Value, keep_secrets: bool) {
    let old = std::mem::take(record);
    let mut data = old["data"].clone();
    if let Value::Object(m) = &mut data {
        for f in &decl.local_fields {
            m.remove(f);
        }
        if !keep_secrets {
            for f in &decl.secret_fields {
                m.remove(f);
            }
        }
    }
    *record = json!({
        "collection": old["collection"],
        "id": old["id"],
        "hlc": old["hlc"],
        "deleted": old["deleted"].as_bool().unwrap_or(false),
        "data": data,
    });
}

/// Split records into batches that fit a sealed call: at most `max_records`
/// each, and about `max_bytes` of JSON.
pub fn batches(records: Vec<Value>, max_records: usize, max_bytes: usize) -> Vec<Vec<Value>> {
    let mut out: Vec<Vec<Value>> = Vec::new();
    let mut size = 0usize;
    for r in records {
        let n = serde_json::to_string(&r).map_or(0, |s| s.len());
        match out.last_mut() {
            Some(last) if last.len() < max_records && size + n <= max_bytes => {
                last.push(r);
                size += n;
            }
            _ => {
                out.push(vec![r]);
                size = n;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> SyncCollection {
        SyncCollection {
            id: "hosts".into(),
            schema_version: 1,
            secret_fields: vec!["password".into()],
            local_fields: vec!["active".into()],
            match_key: vec![],
            guard_fields: vec![],
            refs: vec![],
        }
    }

    #[test]
    fn secret_and_machine_local_fields_are_cut_and_nothing_else_rides_along() {
        let rec = json!({
            "collection": "hosts", "id": "h1", "hlc": "1-0-d", "deleted": false, "smuggled": "x",
            "data": {"name": "web", "password": "pw", "active": true}
        });
        let mut closed = rec.clone();
        strip(&hosts(), &mut closed, false);
        assert_eq!(closed, json!({"collection": "hosts", "id": "h1", "hlc": "1-0-d", "deleted": false, "data": {"name": "web"}}));
        let mut open = rec;
        strip(&hosts(), &mut open, true);
        assert_eq!(open["data"], json!({"name": "web", "password": "pw"}), "secrets stay only when allowed; local fields never");
    }

    #[test]
    fn a_tombstone_has_no_data_to_cut() {
        let mut t = json!({"collection": "hosts", "id": "h1", "hlc": "1-0-d", "deleted": true, "data": null});
        strip(&hosts(), &mut t, false);
        assert_eq!(t["data"], Value::Null);
        assert_eq!(t["deleted"], true);
    }

    #[test]
    fn batches_respect_both_limits() {
        let recs: Vec<Value> = (0..7).map(|i| json!({"id": i, "pad": "x".repeat(100)})).collect();
        let by_count = batches(recs.clone(), 3, usize::MAX);
        assert_eq!(by_count.iter().map(Vec::len).collect::<Vec<_>>(), [3, 3, 1]);
        let by_size = batches(recs, 100, 250);
        assert!(by_size.iter().all(|b| b.len() <= 2), "{:?}", by_size.iter().map(Vec::len).collect::<Vec<_>>());
        assert_eq!(by_size.iter().map(Vec::len).sum::<usize>(), 7);
        assert!(batches(vec![], 3, 10).is_empty());
    }
}
