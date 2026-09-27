//! Resume hint builder — free function used by AgentPool getOrCreate.

use super::traits::DispatchBridgeApi;

// ===== Resume hint builder (free function — used by AgentPool getOrCreate) =====

/// Build a `[System Note]` reminder listing in-flight dispatches under
/// `admin_folder`. Returns `None` when nothing is active. Mirrors
/// `buildDispatchResumeHint` in `src-old/agent/AgentPool.ts:76`.
pub fn build_dispatch_resume_hint(
    bridge: Option<&dyn DispatchBridgeApi>,
    admin_folder: &str,
) -> Option<String> {
    let bridge = bridge?;
    let parents: Vec<_> = bridge
        .get_parents()
        .into_iter()
        .filter(|p| p.admin_folder == admin_folder && p.status == "active")
        .collect();
    if parents.is_empty() {
        return None;
    }
    let mut lines = vec![
        "[System Note] You have previously dispatched the following tasks via dispatch_task. They are still running; do not recreate or redispatch them:".to_string(),
    ];
    for parent in &parents {
        lines.push(format!(
            "- Task group {} (goal: {})",
            parent.id, parent.goal
        ));
        for task in &parent.tasks {
            let preview: String = task.prompt.chars().take(80).collect();
            lines.push(format!(
                "  • [{}] → {}：{}（{}）",
                task.label,
                task.agent_id,
                preview,
                task.status.label()
            ));
        }
    }
    lines.push(
        "You will be notified when these tasks complete. Please wait for results.".to_string(),
    );
    Some(lines.join("\n"))
}
