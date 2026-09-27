//! File change tracking for dispatch tasks.

use super::types::FileChange;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Tracks file changes during task execution.
pub struct FileTracker {
    workspace_dir: PathBuf,
    /// Snapshot of file state before task execution.
    before_snapshot: HashMap<String, FileSnapshot>,
    /// Snapshot of file state after task execution.
    after_snapshot: HashMap<String, FileSnapshot>,
    /// Tracked changes during execution.
    tracked_changes: Vec<FileChange>,
}

#[derive(Clone)]
struct FileSnapshot {
    modified: SystemTime,
    size: u64,
}

impl FileTracker {
    /// Create a new file tracker for the given workspace directory.
    pub fn new(workspace_dir: &Path) -> Self {
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
            before_snapshot: HashMap::new(),
            after_snapshot: HashMap::new(),
            tracked_changes: Vec::new(),
        }
    }

    /// Take a snapshot of the current workspace state.
    pub fn take_snapshot(&mut self) -> anyhow::Result<()> {
        let mut snapshot = HashMap::new();
        self.scan_directory(&self.workspace_dir, &mut snapshot)?;
        self.before_snapshot = snapshot;
        Ok(())
    }

    /// Take a post-execution snapshot and compute changes.
    pub fn finalize(&mut self) -> anyhow::Result<Vec<FileChange>> {
        let mut snapshot = HashMap::new();
        self.scan_directory(&self.workspace_dir, &mut snapshot)?;
        self.after_snapshot = snapshot;

        self.tracked_changes = self.compute_changes();
        Ok(self.tracked_changes.clone())
    }

    /// Get the tracked file changes.
    pub fn get_changes(&self) -> &[FileChange] {
        &self.tracked_changes
    }

    /// Scan a directory recursively and build a snapshot.
    fn scan_directory(
        &self,
        dir: &Path,
        snapshot: &mut HashMap<String, FileSnapshot>,
    ) -> anyhow::Result<()> {
        if !dir.exists() {
            return Ok(());
        }

        let entries = std::fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            // Skip hidden files and directories
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('.'))
                .unwrap_or(false)
            {
                continue;
            }

            // Skip .git directory
            if path.ends_with(".git") {
                continue;
            }

            // Skip node_modules and other common build directories
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name == "node_modules" || name == "target" || name == "dist" || name == "build" {
                    continue;
                }
            }

            let metadata = entry.metadata()?;
            if metadata.is_file() {
                let relative_path = path
                    .strip_prefix(&self.workspace_dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();

                snapshot.insert(
                    relative_path,
                    FileSnapshot {
                        modified: metadata.modified()?,
                        size: metadata.len(),
                    },
                );
            } else if metadata.is_dir() {
                self.scan_directory(&path, snapshot)?;
            }
        }
        Ok(())
    }

    /// Compute file changes between before and after snapshots.
    fn compute_changes(&self) -> Vec<FileChange> {
        let mut changes = Vec::new();
        let all_paths: std::collections::HashSet<&str> = self
            .before_snapshot
            .keys()
            .chain(self.after_snapshot.keys())
            .map(|s| s.as_str())
            .collect();

        for path in all_paths {
            let before = self.before_snapshot.get(path);
            let after = self.after_snapshot.get(path);

            match (before, after) {
                (Some(b), Some(a)) => {
                    // File existed before and after - check if modified
                    if b.modified != a.modified || b.size != a.size {
                        changes.push(FileChange {
                            path: path.to_string(),
                            change_type: "modified".to_string(),
                            lines_added: None,
                            lines_removed: None,
                            summary: Some(format!("Size changed: {} -> {}", b.size, a.size)),
                        });
                    }
                }
                (Some(_), None) => {
                    // File was deleted
                    changes.push(FileChange {
                        path: path.to_string(),
                        change_type: "deleted".to_string(),
                        lines_added: None,
                        lines_removed: None,
                        summary: Some("File deleted".to_string()),
                    });
                }
                (None, Some(a)) => {
                    // File was created
                    changes.push(FileChange {
                        path: path.to_string(),
                        change_type: "created".to_string(),
                        lines_added: None,
                        lines_removed: None,
                        summary: Some(format!("New file, size: {}", a.size)),
                    });
                }
                (None, None) => unreachable!(),
            }
        }

        changes
    }

    /// Manually record a file change (for explicit tracking via tool calls).
    pub fn record_change(&mut self, path: &str, change_type: &str) {
        self.tracked_changes.push(FileChange {
            path: path.to_string(),
            change_type: change_type.to_string(),
            lines_added: None,
            lines_removed: None,
            summary: None,
        });
    }

    /// Record a file change with line count information.
    pub fn record_change_with_lines(
        &mut self,
        path: &str,
        change_type: &str,
        lines_added: i64,
        lines_removed: i64,
    ) {
        self.tracked_changes.push(FileChange {
            path: path.to_string(),
            change_type: change_type.to_string(),
            lines_added: Some(lines_added),
            lines_removed: Some(lines_removed),
            summary: Some(format!(
                "{} lines added, {} lines removed",
                lines_added, lines_removed
            )),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_file_tracker_creation() {
        let temp_dir = TempDir::new().unwrap();
        let tracker = FileTracker::new(temp_dir.path());
        assert_eq!(tracker.get_changes().len(), 0);
    }

    #[test]
    fn test_snapshot_and_changes() {
        let temp_dir = TempDir::new().unwrap();
        let mut tracker = FileTracker::new(temp_dir.path());

        // Take initial snapshot
        tracker.take_snapshot().unwrap();
        assert_eq!(tracker.get_changes().len(), 0);

        // Create a new file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "Hello, World!").unwrap();

        // Finalize and compute changes
        let changes = tracker.finalize().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "test.txt");
        assert_eq!(changes[0].change_type, "created");
    }

    #[test]
    fn test_manual_change_recording() {
        let temp_dir = TempDir::new().unwrap();
        let mut tracker = FileTracker::new(temp_dir.path());

        tracker.record_change("src/main.rs", "modified");
        tracker.record_change_with_lines("src/lib.rs", "created", 100, 0);

        let changes = tracker.get_changes();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "src/main.rs");
        assert_eq!(changes[1].lines_added, Some(100));
    }
}

/// Tools that report the file they just wrote, and the key the path lives
/// under in their result payload. `NotebookEdit` does not use `path` like the
/// other two — hardcoding one key silently loses notebook edits.
const WRITE_TOOLS: &[(&str, &str)] = &[
    ("Write", "path"),
    ("Edit", "path"),
    ("NotebookEdit", "notebook_path"),
];

/// Pull the written path out of a `tool:execution:complete` payload, or `None`
/// when the tool does not write files.
///
/// This is how a dispatch task's writes get attributed **per task even under a
/// shared working directory**: the event is already scoped to one task's agent
/// loop, so unlike a workspace-wide before/after snapshot it cannot pick up a
/// sibling's edits.
///
/// Blind to `Bash`, which can write anywhere and reports no path. Attribution
/// is exact for the three tools above and absent for shell writes.
pub fn written_path_from_tool(
    tool_name: &str,
    content: &serde_json::Value,
) -> Option<(String, &'static str)> {
    let key = WRITE_TOOLS
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, key)| *key)?;
    let path = content.get(key)?.as_str()?.trim();
    if path.is_empty() {
        return None;
    }
    // "modified" rather than "created": these tools report no before-state, and
    // claiming creation for an overwrite would be wrong as often as it is right.
    Some((path.to_string(), "modified"))
}

#[cfg(test)]
mod written_path_tests {
    use super::written_path_from_tool;
    use serde_json::json;

    #[test]
    fn extracts_the_path_each_write_tool_reports() {
        assert_eq!(
            written_path_from_tool("Write", &json!({"path": "/w/a.rs", "size": 12})),
            Some(("/w/a.rs".to_string(), "modified"))
        );
        assert_eq!(
            written_path_from_tool("Edit", &json!({"path": "/w/b.rs", "replacements": 2})),
            Some(("/w/b.rs".to_string(), "modified"))
        );
        // The key that differs — a `path`-only reader loses this one.
        assert_eq!(
            written_path_from_tool("NotebookEdit", &json!({"notebook_path": "/w/c.ipynb"})),
            Some(("/w/c.ipynb".to_string(), "modified"))
        );
    }

    #[test]
    fn ignores_tools_that_do_not_write() {
        assert!(written_path_from_tool("Read", &json!({"path": "/w/a.rs"})).is_none());
        assert!(written_path_from_tool("Grep", &json!({"path": "/w/a.rs"})).is_none());
        // Bash writes but reports no path — the known blind spot, not a bug.
        assert!(written_path_from_tool("Bash", &json!({"command": "echo x > a"})).is_none());
    }

    #[test]
    fn a_missing_or_blank_path_yields_nothing_rather_than_an_empty_entry() {
        assert!(written_path_from_tool("Write", &json!({"size": 3})).is_none());
        assert!(written_path_from_tool("Write", &json!({"path": "   "})).is_none());
        assert!(written_path_from_tool("Edit", &json!({"path": 42})).is_none());
    }
}
