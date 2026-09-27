//! Checklist verification logic for dispatch tasks.

use super::types::{ChecklistItem, DispatchParent, DispatchTask, VerificationResult};

/// Verify a single task's checklist against its result and file changes.
pub fn verify_task_checklist(task: &DispatchTask) -> VerificationResult {
    if task.checklist.is_empty() {
        return VerificationResult {
            verified: true,
            missing_items: Vec::new(),
            failed_items: Vec::new(),
            warnings: vec!["No checklist defined for task".to_string()],
            note: Some("Task completed but has no checklist to verify".to_string()),
        };
    }

    let result_text = task.result.as_deref().unwrap_or("");
    let mut missing_items = Vec::new();
    let mut failed_items = Vec::new();
    let mut warnings = Vec::new();

    // Build dependency graph
    let item_map: std::collections::HashMap<&str, &ChecklistItem> = task
        .checklist
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect();

    // Check each checklist item
    for item in &task.checklist {
        // Check if dependencies are satisfied
        for dep_id in &item.depends_on {
            if let Some(dep_item) = item_map.get(dep_id.as_str()) {
                if dep_item.status != "completed" {
                    missing_items.push(format!(
                        "{}: dependency '{}' not completed",
                        item.description, dep_id
                    ));
                }
            }
        }

        // Check if item is marked as completed
        if item.status != "completed" {
            missing_items.push(format!("{}: not marked as completed", item.description));
            continue;
        }

        // Verify item against task result
        if !item.description.is_empty() {
            let description_lower = item.description.to_lowercase();
            if !result_text.to_lowercase().contains(&description_lower)
                && !verify_in_file_changes(&item.description, &task.file_changes)
            {
                failed_items.push(format!(
                    "{}: not found in task result or file changes",
                    item.description
                ));
            }
        }
    }

    // Check file changes against checklist — same question as
    // `verify_in_file_changes`, so it must use the same comparison or the two
    // disagree about whether an item is backed by a write.
    if !task.file_changes.is_empty() {
        for item in &task.checklist {
            if item.description.contains("file") || item.description.contains("create") {
                if item.status == "completed"
                    && !verify_in_file_changes(&item.description, &task.file_changes)
                {
                    warnings.push(format!(
                        "{}: marked completed but no matching file change found",
                        item.description
                    ));
                }
            }
        }
    }

    // A task that wrote outside what it declared. Advisory while there is no
    // data on how accurate declarations are in practice.
    for path in undeclared_writes(task) {
        warnings.push(format!(
            "wrote {path}, which is outside this task's declared writes"
        ));
    }

    let verified = missing_items.is_empty() && failed_items.is_empty();

    VerificationResult {
        verified,
        missing_items,
        failed_items,
        warnings,
        note: if verified {
            Some("All checklist items verified successfully".to_string())
        } else {
            Some("Task verification failed - see missing/failed items".to_string())
        },
    }
}

/// Verify a parent dispatch's overall checklist completion.
pub fn verify_parent_checklist(parent: &DispatchParent) -> VerificationResult {
    let mut all_items = Vec::new();
    let mut completed_items = Vec::new();
    let mut failed_items = Vec::new();
    let mut warnings = Vec::new();

    // Collect all checklist items from all tasks
    for task in &parent.tasks {
        for item in &task.checklist {
            all_items.push(item.description.clone());
            if item.status == "completed" {
                completed_items.push(item.description.clone());
            } else if item.status == "failed" {
                failed_items.push(format!("{} (task: {})", item.description, task.label));
            }
        }

        // Check task verification results
        if let Some(ref verification) = task.verification_result {
            if !verification.verified {
                warnings.push(format!(
                    "Task '{}' verification failed: {}",
                    task.label,
                    verification.note.as_deref().unwrap_or("unknown reason")
                ));
            }
        }
    }

    let missing_items: Vec<String> = all_items
        .iter()
        .filter(|item| {
            !completed_items.contains(item)
                && !failed_items.iter().any(|f| f.contains(item.as_str()))
        })
        .cloned()
        .collect();

    let verified = missing_items.is_empty() && failed_items.is_empty();

    VerificationResult {
        verified,
        missing_items,
        failed_items,
        warnings,
        note: Some(format!(
            "Parent verification: {}/{} items completed",
            completed_items.len(),
            all_items.len()
        )),
    }
}

/// Does any recorded write back up this checklist item?
///
/// Matches on the **file name** and the last directory, not the whole path.
/// Recorded paths are absolute (`/Users/x/repo/src/agent/pool.rs`) while a
/// checklist item is a sentence someone wrote ("cập nhật pool.rs"), so the
/// old whole-string containment test could essentially never fire — and until
/// writes were recorded at all, the branch was unreachable anyway.
fn verify_in_file_changes(description: &str, file_changes: &[super::types::FileChange]) -> bool {
    let desc_lower = description.to_lowercase();
    for fc in file_changes {
        for part in path_identifiers(&fc.path) {
            // Guard against a one- or two-character component matching half the
            // words in a sentence.
            if part.len() >= 3 && desc_lower.contains(&part) {
                return true;
            }
        }
        if let Some(ref summary) = fc.summary {
            if summary.to_lowercase().contains(&desc_lower) {
                return true;
            }
        }
    }
    false
}

/// The parts of a path worth matching a human sentence against: the file name,
/// its stem, and the directory holding it.
fn path_identifiers(path: &str) -> Vec<String> {
    let p = std::path::Path::new(path);
    let mut out = Vec::new();
    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
        out.push(name.to_lowercase());
    }
    if let Some(stem) = p.file_stem().and_then(|n| n.to_str()) {
        out.push(stem.to_lowercase());
    }
    if let Some(dir) = p
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|n| n.to_str())
    {
        out.push(dir.to_lowercase());
    }
    out
}

/// Paths this task wrote that its own `writes` declaration does not cover.
///
/// Reported as a warning rather than a failure on purpose: the point right now
/// is to learn how wrong declarations are in practice before anything is
/// enforced on the strength of them. A task that declared nothing is not
/// judged — it made no claim to break.
pub fn undeclared_writes(task: &DispatchTask) -> Vec<String> {
    if task.writes.is_empty() || task.file_changes.is_empty() {
        return Vec::new();
    }
    let patterns: Vec<glob::Pattern> = task
        .writes
        .iter()
        .filter_map(|w| glob::Pattern::new(w).ok())
        .collect();
    if patterns.is_empty() {
        return Vec::new();
    }
    task.file_changes
        .iter()
        .filter(|fc| !patterns.iter().any(|p| pattern_covers(p, &fc.path)))
        .map(|fc| fc.path.clone())
        .collect()
}

/// Match a declared glob against a recorded path, trying the path as written
/// and with each leading directory removed — declarations are written relative
/// to the workspace while recorded paths are absolute.
fn pattern_covers(pattern: &glob::Pattern, path: &str) -> bool {
    let mut rest = path.trim_start_matches('/');
    loop {
        if pattern.matches(rest) {
            return true;
        }
        match rest.find('/') {
            Some(i) => rest = &rest[i + 1..],
            None => return false,
        }
    }
}

/// Auto-generate checklist items from a task prompt.
pub fn generate_checklist_from_prompt(prompt: &str) -> Vec<ChecklistItem> {
    let mut items = Vec::new();
    let lines: Vec<&str> = prompt.lines().collect();

    // Look for numbered lists or bullet points in the prompt
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // Match numbered lists: "1.", "2.", etc.
        if let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit()) {
            if rest.starts_with('.') || rest.starts_with(')') {
                let description = rest[1..]
                    .trim()
                    .trim_start_matches(')')
                    .trim_start_matches('.')
                    .trim();
                if !description.is_empty() {
                    items.push(ChecklistItem {
                        id: format!("item-{}", i),
                        description: description.to_string(),
                        status: "pending".to_string(),
                        depends_on: Vec::new(),
                        verification_note: None,
                    });
                }
            }
        }

        // Match bullet points: "-", "*"
        if trimmed.starts_with('-') || trimmed.starts_with('*') {
            let description = trimmed[1..].trim();
            if !description.is_empty() {
                items.push(ChecklistItem {
                    id: format!("item-{}", i),
                    description: description.to_string(),
                    status: "pending".to_string(),
                    depends_on: Vec::new(),
                    verification_note: None,
                });
            }
        }
    }

    // If no structured list found, create a single item from the first sentence
    if items.is_empty() {
        if let Some(first_sentence) = prompt.split('.').next() {
            let description = first_sentence.trim();
            if !description.is_empty() {
                items.push(ChecklistItem {
                    id: "item-0".to_string(),
                    description: description.to_string(),
                    status: "pending".to_string(),
                    depends_on: Vec::new(),
                    verification_note: None,
                });
            }
        }
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_task_with_empty_checklist() {
        let task = DispatchTask {
            id: "test-1".to_string(),
            label: "Test Task".to_string(),
            agent_id: "agent-1".to_string(),
            agent_jid: "jid-1".to_string(),
            depends_on: Vec::new(),
            prompt: "Test prompt".to_string(),
            status: crate::agent::dispatch_bridge::types::DispatchTaskStatus::Done,
            result: Some("Task completed successfully".to_string()),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            started_at: None,
            timeout_seconds: 900,
            timeout_at: None,
            completed_at: None,
            is_virtual: false,
            persona_name: None,
            checklist: Vec::new(),
            checklist_auto: false,
            retry_count: 0,
            file_changes: Vec::new(),
            verification_result: None,
            io: Default::default(),
            writes: Vec::new(),
            isolation: Default::default(),
            worktree: None,
        };

        let result = verify_task_checklist(&task);
        assert!(result.verified);
        assert_eq!(result.warnings.len(), 1);
    }

    #[test]
    fn test_generate_checklist_from_prompt() {
        let prompt =
            "Implement feature X:\n1. Create new file\n2. Modify existing code\n3. Add tests";
        let checklist = generate_checklist_from_prompt(prompt);
        assert_eq!(checklist.len(), 3);
        assert_eq!(checklist[0].description, "Create new file");
        assert_eq!(checklist[1].description, "Modify existing code");
        assert_eq!(checklist[2].description, "Add tests");
    }
}

#[cfg(test)]
mod write_verification_tests {
    use super::*;
    use crate::agent::dispatch_bridge::types::{DispatchTaskStatus, FileChange, TaskIo};

    fn task_with(writes: &[&str], wrote: &[&str]) -> DispatchTask {
        DispatchTask {
            id: "d1".into(),
            label: "l".into(),
            agent_id: "a".into(),
            agent_jid: "j".into(),
            depends_on: vec![],
            prompt: "p".into(),
            status: DispatchTaskStatus::Done,
            result: Some(String::new()),
            created_at: "x".into(),
            started_at: None,
            timeout_seconds: 0,
            timeout_at: None,
            completed_at: None,
            is_virtual: false,
            persona_name: None,
            checklist: vec![],
            checklist_auto: false,
            retry_count: 0,
            file_changes: wrote
                .iter()
                .map(|p| FileChange {
                    path: p.to_string(),
                    change_type: "modified".into(),
                    lines_added: None,
                    lines_removed: None,
                    summary: None,
                })
                .collect(),
            verification_result: None,
            io: TaskIo::Exclusive,
            writes: writes.iter().map(|s| s.to_string()).collect(),
            isolation: Default::default(),
            worktree: None,
        }
    }

    #[test]
    fn a_write_inside_the_declaration_raises_nothing() {
        let t = task_with(
            &["src/agent/**"],
            &[
                "/Users/x/repo/src/agent/pool.rs",
                "/Users/x/repo/src/agent/a.rs",
            ],
        );
        assert!(undeclared_writes(&t).is_empty());
    }

    #[test]
    fn a_write_outside_the_declaration_is_reported() {
        let t = task_with(&["src/agent/**"], &["/Users/x/repo/src/mcp/server.rs"]);
        assert_eq!(
            undeclared_writes(&t),
            vec!["/Users/x/repo/src/mcp/server.rs"]
        );
    }

    #[test]
    fn a_task_that_declared_nothing_is_not_judged() {
        // It made no claim, so there is nothing to have broken.
        let t = task_with(&[], &["/Users/x/repo/anything.rs"]);
        assert!(undeclared_writes(&t).is_empty());
    }

    #[test]
    fn an_exact_file_declaration_matches_its_absolute_path() {
        let t = task_with(&["docs/x.md"], &["/Users/x/repo/docs/x.md"]);
        assert!(undeclared_writes(&t).is_empty());
    }

    #[test]
    fn a_checklist_item_is_backed_by_the_file_it_names() {
        // The comparison that could never fire before: an absolute path against
        // a sentence naming just the file.
        let changes = [FileChange {
            path: "/Users/x/repo/src/agent/pool.rs".into(),
            change_type: "modified".into(),
            lines_added: None,
            lines_removed: None,
            summary: None,
        }];
        assert!(verify_in_file_changes(
            "cập nhật pool.rs cho đúng",
            &changes
        ));
        assert!(verify_in_file_changes(
            "refactor the agent module",
            &changes
        ));
        assert!(!verify_in_file_changes(
            "update the web dashboard",
            &changes
        ));
    }

    #[test]
    fn a_very_short_path_component_does_not_match_everything() {
        let changes = [FileChange {
            path: "/w/a/b.rs".into(),
            change_type: "modified".into(),
            lines_added: None,
            lines_removed: None,
            summary: None,
        }];
        // "a" and "b" are too short to be evidence of anything.
        assert!(!verify_in_file_changes("write a report about b", &changes));
    }
}
