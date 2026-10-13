//! Where datasets land: `<root>/<name>/data.jsonl` + `manifest.json`, with
//! `<root>` = `$SENCLAW_DATA_HOME/datasets` in the daemon.
//!
//! Records are appended and flushed one line at a time, so a crash or a
//! cancel keeps everything accepted so far (CAMEL rewrites the whole output
//! file on every record). The manifest is written at start and replaced
//! atomically at the end; one still saying `running` with no live job is a
//! run the daemon did not finish.
//!
//! The directory is 0700 and its files 0600, like trajectories: an export of
//! a chat carries the contents of files the agent read.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

pub const DATA_FILE: &str = "data.jsonl";
pub const MANIFEST_FILE: &str = "manifest.json";

pub fn default_root() -> PathBuf {
    crate::util::paths::senclaw_data_home().join("datasets")
}

/// A folder name from free text: diacritics folded, anything else outside
/// `[a-z0-9-_]` turned into `-`. "Bộ eval Router" → `bo-eval-router`.
pub fn sanitize_name(name: &str) -> String {
    let folded = crate::security::replication::fold(name.trim());
    let mut out = String::new();
    for c in folded.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').chars().take(60).collect::<String>();
    if out.is_empty() {
        "dataset".into()
    } else {
        out
    }
}

pub struct DatasetWriter {
    dir: PathBuf,
    data: File,
}

impl DatasetWriter {
    /// Create `<root>/<name>`, or `<name>-2`, `-3`… when taken. Never writes
    /// into an existing dataset.
    pub fn create(root: &Path, name: &str) -> io::Result<DatasetWriter> {
        create_private_dir(root)?;
        let base = sanitize_name(name);
        let mut n = 1;
        let dir = loop {
            let candidate = if n == 1 { root.join(&base) } else { root.join(format!("{base}-{n}")) };
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
                Err(e) => return Err(e),
            }
        };
        set_mode(&dir, 0o700)?;
        let data = private_file(&dir.join(DATA_FILE), true)?;
        Ok(DatasetWriter { dir, data })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn name(&self) -> String {
        self.dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn append(&mut self, record: &Value) -> io::Result<()> {
        let mut line = serde_json::to_string(record).map_err(io::Error::other)?;
        line.push('\n');
        self.data.write_all(line.as_bytes())?;
        self.data.flush()
    }

}

/// Replace `<dir>/manifest.json` atomically (temp file + rename).
pub fn write_manifest(dir: &Path, manifest: &Value) -> io::Result<()> {
    let tmp = dir.join(".manifest.json.tmp");
    {
        let mut f = private_file(&tmp, false)?;
        f.write_all(&serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?)?;
        f.sync_all()?;
    }
    fs::rename(tmp, dir.join(MANIFEST_FILE))
}

/// The folder of dataset `name` under `root`, when it exists. A name is
/// only ever a folder name this module made, so anything else — a path, a
/// `..` — names no dataset.
pub fn dataset_dir(root: &Path, name: &str) -> Option<PathBuf> {
    let valid = !name.is_empty()
        && name.len() <= 80
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    let dir = root.join(name);
    (valid && dir.join(MANIFEST_FILE).is_file()).then_some(dir)
}

/// The dataset's manifest.
pub fn read_manifest(dir: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(dir.join(MANIFEST_FILE)).ok()?).ok()
}

/// A page of records (`offset`, at most `limit`, at most `max_bytes` of
/// them — one exported turn can be tens of KB) and the total count. The
/// first record of a page is always included. Unreadable lines are skipped,
/// not fatal: a run cut off mid-write leaves one partial last line.
pub fn read_records(dir: &Path, offset: usize, limit: usize, max_bytes: usize) -> io::Result<(Vec<Value>, usize)> {
    use std::io::BufRead;
    let f = File::open(dir.join(DATA_FILE))?;
    let mut total = 0;
    let mut page = Vec::new();
    let mut bytes = 0;
    for line in io::BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if total >= offset && page.len() < limit && (page.is_empty() || bytes + line.len() <= max_bytes) {
            if let Ok(v) = serde_json::from_str(&line) {
                bytes += line.len();
                page.push(v);
            }
        }
        total += 1;
    }
    Ok((page, total))
}

/// Remove a dataset folder and everything in it.
pub fn delete(dir: &Path) -> io::Result<()> {
    fs::remove_dir_all(dir)
}

/// Every dataset's manifest under `root`, newest first. Folders without a
/// readable manifest are skipped.
pub fn list(root: &Path) -> Vec<Value> {
    let Ok(rd) = fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<Value> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| fs::read(e.path().join(MANIFEST_FILE)).ok())
        .filter_map(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .collect();
    out.sort_by_key(|m| std::cmp::Reverse(m.get("startedAt").and_then(Value::as_u64).unwrap_or(0)));
    out
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    set_mode(dir, 0o700)
}

fn private_file(path: &Path, append: bool) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.create(true).write(true);
    if append {
        o.append(true);
    } else {
        o.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_fold_diacritics_and_drop_punctuation() {
        assert_eq!(sanitize_name("Bộ eval Router!"), "bo-eval-router");
        assert_eq!(sanitize_name("../../etc"), "etc");
        assert_eq!(sanitize_name("  "), "dataset");
    }

    #[test]
    fn appends_lines_and_never_reuses_a_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let mut a = DatasetWriter::create(tmp.path(), "eval").unwrap();
        a.append(&json!({"text": "one"})).unwrap();
        a.append(&json!({"text": "two"})).unwrap();
        write_manifest(a.dir(), &json!({"name": "eval", "startedAt": 1})).unwrap();
        let b = DatasetWriter::create(tmp.path(), "eval").unwrap();
        write_manifest(b.dir(), &json!({"name": "eval-2", "startedAt": 2})).unwrap();

        assert_eq!(b.name(), "eval-2");
        let data = fs::read_to_string(a.dir().join(DATA_FILE)).unwrap();
        assert_eq!(data.lines().count(), 2);
        let names: Vec<_> = list(tmp.path()).iter().map(|m| m["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, vec!["eval-2", "eval"], "newest first");
    }

    #[test]
    fn records_page_and_names_cannot_escape_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = DatasetWriter::create(tmp.path(), "page").unwrap();
        for i in 0..5 {
            w.append(&json!({ "i": i })).unwrap();
        }
        write_manifest(w.dir(), &json!({})).unwrap();
        let dir = dataset_dir(tmp.path(), "page").unwrap();
        let (page, total) = read_records(&dir, 1, 2, usize::MAX).unwrap();
        assert_eq!(total, 5);
        assert_eq!(page, vec![json!({"i": 1}), json!({"i": 2})]);
        let (page, _) = read_records(&dir, 0, 5, 1).unwrap();
        assert_eq!(page.len(), 1, "a byte cap still returns the first record");
        assert!(dataset_dir(tmp.path(), "../page").is_none());
        assert!(dataset_dir(tmp.path(), "Page").is_none());
        assert!(dataset_dir(tmp.path(), "missing").is_none());
        delete(&dir).unwrap();
        assert!(dataset_dir(tmp.path(), "page").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn dataset_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let w = DatasetWriter::create(tmp.path(), "x").unwrap();
        write_manifest(w.dir(), &json!({})).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(w.dir()), 0o700);
        assert_eq!(mode(&w.dir().join(DATA_FILE)), 0o600);
        assert_eq!(mode(&w.dir().join(MANIFEST_FILE)), 0o600);
    }
}
