//! Daily memory log. Mirrors `src-old/memory/DailyLogger.ts`.
//!
//! Each agent folder gets one markdown file per day under
//! `agentsDir/{folder}/memory/YYYY-MM-DD.md`. After every append we trim the
//! directory to the most recent [`MAX_DAYS`] entries (FIFO).

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use chrono::Local;

use crate::util::local_time::{local_date_string, local_time_string};

pub const MAX_DAYS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Assistant => "Assistant",
        }
    }
}

pub struct DailyLogger {
    agents_dir: PathBuf,
}

impl DailyLogger {
    pub fn new(agents_dir: PathBuf) -> Self {
        Self { agents_dir }
    }

    /// Append one entry to today's log. Empty content is silently dropped.
    /// Errors are logged but not propagated — logging must never break the
    /// caller flow (matches TS behavior).
    pub fn append(&self, folder: &str, role: Role, content: &str) {
        if content.trim().is_empty() {
            return;
        }
        if let Err(e) = self.try_append(folder, role, content) {
            tracing::warn!(
                folder, role = role.label(), error = %e,
                "[DailyLogger] failed to append entry"
            );
        }
    }

    fn try_append(&self, folder: &str, role: Role, content: &str) -> Result<()> {
        let mem_dir = self.memory_dir(folder);
        fs::create_dir_all(&mem_dir)?;

        let now = Local::now();
        let today = local_date_string(now);
        let time = local_time_string(now);
        let log_file = mem_dir.join(format!("{today}.md"));

        let mut buf = String::new();
        if !log_file.exists() {
            buf.push_str(&format!("# {today}\n"));
        }
        buf.push_str(&format!(
            "\n## {time} [{}]\n\n{}\n",
            role.label(),
            content.trim()
        ));

        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)?;
        f.write_all(buf.as_bytes())?;
        drop(f);

        self.cleanup(folder);
        Ok(())
    }

    /// Drop the oldest log files so that at most [`MAX_DAYS`] remain.
    /// Errors are silently swallowed (matches TS — best-effort housekeeping).
    pub fn cleanup(&self, folder: &str) {
        let mem_dir = self.memory_dir(folder);
        let Ok(entries) = fs::read_dir(&mem_dir) else {
            return;
        };

        let mut files: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_owned()))
            .filter(|f| is_date_filename(f))
            .collect();

        if files.len() <= MAX_DAYS {
            return;
        }
        files.sort();
        let drop_count = files.len() - MAX_DAYS;
        for f in files.into_iter().take(drop_count) {
            let _ = fs::remove_file(mem_dir.join(f));
        }
    }

    fn memory_dir(&self, folder: &str) -> PathBuf {
        self.agents_dir.join(folder).join("memory")
    }
}

fn is_date_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".md") else {
        return false;
    };
    if stem.len() != 10 {
        return false;
    }
    let bytes = stem.as_bytes();
    bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[8..10].iter().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn date_filename_check() {
        assert!(is_date_filename("2026-04-28.md"));
        assert!(!is_date_filename("2026-04-28.txt"));
        assert!(!is_date_filename("hello.md"));
        assert!(!is_date_filename("2026/04/28.md"));
    }

    fn read_today(dir: &Path, folder: &str) -> String {
        let today = local_date_string(Local::now());
        let file = dir.join(folder).join("memory").join(format!("{today}.md"));
        fs::read_to_string(file).unwrap()
    }

    #[test]
    fn append_creates_dir_and_writes_header() {
        let tmp = TempDir::new().unwrap();
        let logger = DailyLogger::new(tmp.path().to_path_buf());
        logger.append("g1", Role::User, "hello world");
        let content = read_today(tmp.path(), "g1");
        assert!(content.starts_with("# "));
        assert!(content.contains("[User]"));
        assert!(content.contains("hello world"));
    }

    #[test]
    fn empty_content_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let logger = DailyLogger::new(tmp.path().to_path_buf());
        logger.append("g1", Role::User, "   ");
        let mem_dir = tmp.path().join("g1").join("memory");
        // Memory dir must not have been created.
        assert!(!mem_dir.exists() || fs::read_dir(&mem_dir).unwrap().next().is_none());
    }

    #[test]
    fn append_skips_header_when_file_exists() {
        let tmp = TempDir::new().unwrap();
        let logger = DailyLogger::new(tmp.path().to_path_buf());
        logger.append("g1", Role::User, "first");
        logger.append("g1", Role::Assistant, "second");
        let content = read_today(tmp.path(), "g1");
        // exactly one h1 header
        assert_eq!(
            content.matches("\n# ").count() + content.starts_with("# ") as usize,
            1
        );
        assert!(content.contains("first"));
        assert!(content.contains("second"));
        assert!(content.contains("[User]"));
        assert!(content.contains("[Assistant]"));
    }

    #[test]
    fn cleanup_drops_oldest_when_over_limit() {
        let tmp = TempDir::new().unwrap();
        let logger = DailyLogger::new(tmp.path().to_path_buf());
        let mem_dir = tmp.path().join("g1").join("memory");
        fs::create_dir_all(&mem_dir).unwrap();

        // Pre-create MAX_DAYS + 5 fake date files.
        for i in 0..(MAX_DAYS + 5) {
            let name = format!("2024-{:02}-{:02}.md", (i / 28) + 1, (i % 28) + 1);
            fs::write(mem_dir.join(&name), "x").unwrap();
        }
        logger.cleanup("g1");
        let remaining = fs::read_dir(&mem_dir).unwrap().count();
        assert_eq!(remaining, MAX_DAYS);
    }

    #[test]
    fn cleanup_leaves_untouched_under_limit() {
        let tmp = TempDir::new().unwrap();
        let logger = DailyLogger::new(tmp.path().to_path_buf());
        let mem_dir = tmp.path().join("g1").join("memory");
        fs::create_dir_all(&mem_dir).unwrap();
        for i in 0..3 {
            fs::write(mem_dir.join(format!("2024-01-0{}.md", i + 1)), "x").unwrap();
        }
        logger.cleanup("g1");
        assert_eq!(fs::read_dir(&mem_dir).unwrap().count(), 3);
    }
}
