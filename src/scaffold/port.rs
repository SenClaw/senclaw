//! Picking a port for a new Space App.
//!
//! Two apps on one port is a failure with no good error message: the second one
//! dies at bind time, the daemon's health check times out, and the UI says the
//! app did not start. Cheap to avoid at create time, so this checks both things
//! that can collide:
//!
//! - **Declared** ports — every installed app's `senclaw-manifest.json`, plus
//!   any sibling app in the directory being scaffolded into. An app that is
//!   stopped still owns its port, and a session app is stopped most of the time.
//! - **Bound** ports — anything currently listening, app or not.

use std::collections::BTreeSet;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

/// Where user-created apps start. The apps SenClaw ships sit in 4300–4799, so
/// starting above them keeps a hand-made app from colliding with one that gets
/// installed later.
pub const RANGE_START: u16 = 4800;
pub const RANGE_END: u16 = 4999;

/// Ports declared by every app manifest under `dir` (one level deep).
pub fn declared_ports(dir: &Path) -> BTreeSet<u16> {
    let mut used = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return used;
    };
    for e in entries.flatten() {
        let manifest = e.path().join("senclaw-manifest.json");
        if let Some(p) = port_of(&manifest) {
            used.insert(p);
        }
    }
    used
}

fn port_of(manifest: &Path) -> Option<u16> {
    let raw = std::fs::read_to_string(manifest).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("runtime")?.get("port")?.as_u64()?.try_into().ok()
}

/// True when nothing is listening on the port right now.
///
/// Binds 127.0.0.1 specifically: that is where a Space App listens, and a
/// service bound to another interface is not a conflict.
fn is_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Choose a port, skipping everything declared in `search_dirs` and everything
/// currently listening.
///
/// Returns `None` only when the whole range is taken — 200 apps, at which point
/// the user should be passing `--port` anyway.
pub fn pick(search_dirs: &[PathBuf]) -> Option<u16> {
    let mut used = BTreeSet::new();
    for d in search_dirs {
        used.extend(declared_ports(d));
    }
    (RANGE_START..=RANGE_END).find(|p| !used.contains(p) && is_free(*p))
}

/// The directories worth scanning for neighbours: the installed Space Apps, and
/// wherever the new project is being written.
pub fn search_dirs(config: &crate::config::Config, dest_parent: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![config.paths.workspace_dir.join("space-apps")];
    let dest = dest_parent.to_path_buf();
    if !dirs.contains(&dest) {
        dirs.push(dest);
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn app(dir: &Path, id: &str, port: Option<u16>) {
        let d = dir.join(id);
        std::fs::create_dir_all(&d).unwrap();
        let runtime = match port {
            Some(p) => format!(r#"{{"port":{p}}}"#),
            None => "{}".to_string(),
        };
        std::fs::write(
            d.join("senclaw-manifest.json"),
            format!(r#"{{"id":"{id}","runtime":{runtime}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn reads_ports_from_neighbouring_manifests() {
        let td = TempDir::new().unwrap();
        app(td.path(), "a", Some(4800));
        app(td.path(), "b", Some(4802));
        app(td.path(), "c", None);
        std::fs::create_dir_all(td.path().join("not-an-app")).unwrap();

        let used = declared_ports(td.path());
        assert_eq!(used.iter().copied().collect::<Vec<_>>(), [4800, 4802]);
    }

    #[test]
    fn a_malformed_manifest_does_not_abort_the_scan() {
        let td = TempDir::new().unwrap();
        let bad = td.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("senclaw-manifest.json"), "{not json").unwrap();
        app(td.path(), "good", Some(4805));

        assert_eq!(
            declared_ports(td.path()).iter().copied().collect::<Vec<_>>(),
            [4805]
        );
    }

    #[test]
    fn skips_declared_ports_when_picking() {
        let td = TempDir::new().unwrap();
        // Claim the bottom of the range so the pick has to move past it.
        for (i, p) in (RANGE_START..RANGE_START + 5).enumerate() {
            app(td.path(), &format!("app{i}"), Some(p));
        }
        let picked = pick(&[td.path().to_path_buf()]).unwrap();
        assert!(
            picked >= RANGE_START + 5,
            "picked {picked}, which is already declared"
        );
        assert!(picked <= RANGE_END);
    }

    #[test]
    fn skips_a_port_that_is_actually_listening() {
        let td = TempDir::new().unwrap();
        // Hold a socket open for the duration of the test.
        let Ok(listener) = TcpListener::bind(("127.0.0.1", 0)) else {
            return; // no loopback in this sandbox; nothing to assert
        };
        let held = listener.local_addr().unwrap().port();
        assert!(!is_free(held), "a bound port must not read as free");
        drop(td);
        drop(listener);
    }
}
