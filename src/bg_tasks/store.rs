//! `bg-tasks.json`: the running tasks, so a daemon that died without cleaning up
//! (SIGKILL, a crash, a power cut) still knows what it left behind.
//!
//! The commands may carry secrets (a token in an environment prefix), so the file
//! is 0600. Only running tasks are kept; a graceful shutdown stops them and
//! writes an empty list.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub id: String,
    pub chat_jid: String,
    pub description: String,
    pub command: String,
    pub cwd: String,
    #[serde(default)]
    pub isolation: String,
    pub started_ms: i64,
    pub pgid: u32,
    #[serde(default)]
    pub pids: Vec<u32>,
}

/// An unreadable or missing file is an empty list: a task that cannot be found
/// again is lost, never a reason to refuse to boot.
pub fn load(path: &Path) -> Vec<Record> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, records: &[Record]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(records)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str) -> Record {
        Record {
            id: id.into(),
            chat_jid: "web:a".into(),
            description: "dev server".into(),
            command: "nohup npx vite &".into(),
            cwd: "/tmp/x".into(),
            isolation: "seatbelt".into(),
            started_ms: 5,
            pgid: 4242,
            pids: vec![4242, 4243],
        }
    }

    #[test]
    fn round_trips_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("bg-tasks.json");
        save(&path, &[rec("bash-00001"), rec("bash-00002")]).unwrap();
        assert_eq!(load(&path), vec![rec("bash-00001"), rec("bash-00002")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_missing_or_corrupt_file_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bg-tasks.json");
        assert!(load(&path).is_empty());
        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&path).is_empty());
    }
}
