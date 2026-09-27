//! Verification reporting for dispatch tasks and parents.

use super::types::{DispatchParent, DispatchTask, VerificationResult};

/// Generate a human-readable verification report for a task.
pub fn generate_task_verification_report(task: &DispatchTask) -> String {
    let mut report = format!("### Verification Report: Task '{}'\n\n", task.label);
    report.push_str(&format!("**Status:** {}\n\n", task.status.label()));

    if let Some(ref verification) = task.verification_result {
        report.push_str(&format!("**Verified:** {}\n\n", verification.verified));

        if let Some(note) = &verification.note {
            report.push_str(&format!("**Note:** {}\n\n", note));
        }

        if !verification.missing_items.is_empty() {
            report.push_str("## Missing Items\n\n");
            for item in &verification.missing_items {
                report.push_str(&format!("- ❌ {}\n", item));
            }
            report.push('\n');
        }

        if !verification.failed_items.is_empty() {
            report.push_str("## Failed Items\n\n");
            for item in &verification.failed_items {
                report.push_str(&format!("- ❌ {}\n", item));
            }
            report.push('\n');
        }

        if !verification.warnings.is_empty() {
            report.push_str("## Warnings\n\n");
            for warning in &verification.warnings {
                report.push_str(&format!("- ⚠️ {}\n", warning));
            }
            report.push('\n');
        }

        if verification.verified {
            report.push_str("## Summary\n\n✅ All checklist items verified successfully.\n\n");
        } else {
            report.push_str("## Summary\n\n❌ Verification failed. Please review the missing and failed items above.\n\n");

            report.push_str("## Recommended Actions\n\n");
            if !verification.missing_items.is_empty() {
                report.push_str("- Mark missing checklist items as completed if they were done\n");
                report.push_str(
                    "- Update checklist dependencies if items were completed out of order\n",
                );
            }
            if !verification.failed_items.is_empty() {
                report.push_str(
                    "- Review failed items and ensure they are reflected in task output\n",
                );
                report.push_str(
                    "- Add missing content to task result or update checklist descriptions\n",
                );
            }
            report.push_str(
                "- Use `update_checklist_status` to manually adjust item statuses if needed\n\n",
            );
        }
    } else {
        report.push_str(
            "**Note:** No verification result available. Verification may not have run yet.\n\n",
        );
    }

    // Add checklist status
    if !task.checklist.is_empty() {
        report.push_str("## Checklist Status\n\n");
        let completed = task
            .checklist
            .iter()
            .filter(|i| i.status == "completed")
            .count();
        let pending = task
            .checklist
            .iter()
            .filter(|i| i.status == "pending")
            .count();
        let failed = task
            .checklist
            .iter()
            .filter(|i| i.status == "failed")
            .count();

        report.push_str(&format!("- Total items: {}\n", task.checklist.len()));
        report.push_str(&format!("- ✅ Completed: {}\n", completed));
        report.push_str(&format!("- ⏳ Pending: {}\n", pending));
        if failed > 0 {
            report.push_str(&format!("- ❌ Failed: {}\n", failed));
        }
        report.push('\n');

        report.push_str("### Items\n\n");
        for item in &task.checklist {
            let icon = match item.status.as_str() {
                "completed" => "✅",
                "failed" => "❌",
                _ => "⏳",
            };
            report.push_str(&format!(
                "{} {} - {}\n",
                icon, item.description, item.status
            ));
            if let Some(note) = &item.verification_note {
                report.push_str(&format!("  *Note: {}*\n", note));
            }
        }
        report.push('\n');
    }

    // Add file changes
    if !task.file_changes.is_empty() {
        report.push_str("## File Changes\n\n");
        report.push_str(&format!("Total changes: {}\n\n", task.file_changes.len()));

        for change in &task.file_changes {
            let icon = match change.change_type.as_str() {
                "created" => "➕",
                "modified" => "✏️",
                "deleted" => "🗑️",
                _ => "📝",
            };
            report.push_str(&format!(
                "{} **{}** ({})\n",
                icon, change.path, change.change_type
            ));
            if let Some(summary) = &change.summary {
                report.push_str(&format!("  - {}\n", summary));
            }
            if let Some(lines_added) = change.lines_added {
                report.push_str(&format!("  - Lines added: {}\n", lines_added));
            }
            if let Some(lines_removed) = change.lines_removed {
                report.push_str(&format!("  - Lines removed: {}\n", lines_removed));
            }
        }
        report.push('\n');
    }

    report
}

/// Generate a human-readable verification report for a parent dispatch.
pub fn generate_parent_verification_report(parent: &DispatchParent) -> String {
    let mut report = format!("### Verification Report: Parent '{}'\n\n", parent.id);
    report.push_str(&format!("**Goal:** {}\n\n", parent.goal));
    report.push_str(&format!("**Status:** {}\n\n", parent.status));

    if let Some(ref completed_at) = parent.completed_at {
        report.push_str(&format!("**Completed at:** {}\n\n", completed_at));
    }

    // Overall verification
    let all_verifications: Vec<&VerificationResult> = parent
        .tasks
        .iter()
        .filter_map(|t| t.verification_result.as_ref())
        .collect();

    if !all_verifications.is_empty() {
        let verified_count = all_verifications.iter().filter(|v| v.verified).count();
        report.push_str(&format!(
            "## Overall Verification\n\n**Tasks verified:** {}/{}\n\n",
            verified_count,
            all_verifications.len()
        ));

        if verified_count == all_verifications.len() {
            report.push_str("✅ All tasks verified successfully.\n\n");
        } else {
            report.push_str("⚠️ Some tasks failed verification. See task details below.\n\n");
        }
    }

    // Task summaries
    report.push_str("## Task Summary\n\n");
    for task in &parent.tasks {
        let status_icon = match task.status.label() {
            "done" => "✅",
            "error" => "❌",
            "timeout" => "⏰",
            _ => "⏳",
        };

        let verification_icon = if let Some(ref v) = task.verification_result {
            if v.verified {
                "✅"
            } else {
                "❌"
            }
        } else {
            "❓"
        };

        let checklist_status = if task.checklist.is_empty() {
            "No checklist".to_string()
        } else {
            let completed = task
                .checklist
                .iter()
                .filter(|i| i.status == "completed")
                .count();
            format!("{}/{} completed", completed, task.checklist.len())
        };

        report.push_str(&format!(
            "{} **{}** (status: {}, verification: {}, checklist: {})\n",
            status_icon,
            task.label,
            task.status.label(),
            verification_icon,
            checklist_status
        ));

        if !task.file_changes.is_empty() {
            report.push_str(&format!("  - File changes: {}\n", task.file_changes.len()));
        }
    }
    report.push('\n');

    // Detailed task reports
    report.push_str("## Detailed Task Reports\n\n");
    for task in &parent.tasks {
        report.push_str(&generate_task_verification_report(task));
        report.push_str("---\n\n");
    }

    report
}

/// Generate a summary of actionable next steps based on verification results.
pub fn generate_actionable_next_steps(parent: &DispatchParent) -> String {
    let mut steps = Vec::new();

    for task in &parent.tasks {
        if let Some(ref verification) = task.verification_result {
            if !verification.verified {
                steps.push(format!(
                    "Task '{}': {}",
                    task.label,
                    verification
                        .note
                        .as_deref()
                        .unwrap_or("verification failed")
                ));

                for item in &verification.missing_items {
                    steps.push(format!("  - Complete: {}", item));
                }

                for item in &verification.failed_items {
                    steps.push(format!("  - Fix: {}", item));
                }
            }
        }

        // Check for pending checklist items in completed tasks
        if task.status.label() == "done" {
            let pending: Vec<&str> = task
                .checklist
                .iter()
                .filter(|i| i.status == "pending")
                .map(|i| i.description.as_str())
                .collect();

            if !pending.is_empty() {
                steps.push(format!(
                    "Task '{}' has {} pending checklist items despite being done:",
                    task.label,
                    pending.len()
                ));
                for item in pending {
                    steps.push(format!("  - Mark as completed: {}", item));
                }
            }
        }
    }

    if steps.is_empty() {
        "✅ All tasks verified successfully. No action needed.".to_string()
    } else {
        let mut report = "## Actionable Next Steps\n\n".to_string();
        for step in steps {
            report.push_str(&format!("{}\n", step));
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::dispatch_bridge::types::{ChecklistItem, DispatchTaskStatus};

    #[test]
    fn test_generate_task_verification_report() {
        let task = DispatchTask {
            id: "test-1".to_string(),
            label: "Test Task".to_string(),
            agent_id: "agent-1".to_string(),
            agent_jid: "jid-1".to_string(),
            depends_on: Vec::new(),
            prompt: "Test prompt".to_string(),
            status: DispatchTaskStatus::Done,
            result: Some("Task completed successfully".to_string()),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            started_at: None,
            timeout_seconds: 900,
            timeout_at: None,
            completed_at: None,
            is_virtual: false,
            persona_name: None,
            checklist: vec![ChecklistItem {
                id: "item-1".to_string(),
                description: "Complete task".to_string(),
                status: "completed".to_string(),
                depends_on: Vec::new(),
                verification_note: None,
            }],
            checklist_auto: false,
            retry_count: 0,
            file_changes: Vec::new(),
            verification_result: None,
            io: Default::default(),
            writes: Vec::new(),
            isolation: Default::default(),
            worktree: None,
        };

        let report = generate_task_verification_report(&task);
        assert!(report.contains("Test Task"));
        assert!(report.contains("completed"));
    }

    #[test]
    fn test_generate_actionable_next_steps() {
        let parent = DispatchParent {
            id: "p-1".to_string(),
            goal: "Test goal".to_string(),
            admin_folder: "main".to_string(),
            chat_jid: None,
            shared_workspace: None,
            status: "done".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            completed_at: None,
            tasks: vec![],
        };

        let steps = generate_actionable_next_steps(&parent);
        assert!(steps.contains("No action needed"));
    }
}
