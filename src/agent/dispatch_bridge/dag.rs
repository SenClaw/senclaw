//! DAG helper functions for dispatch task scheduling.

use super::types::{DispatchParent, DispatchTask, DispatchTaskStatus};

// ===== DAG helpers =====

/// True when every task referenced in `task.depends_on` has reached a terminal
/// status. Continue-on-error semantics: error/timeout still unblock dependants.
pub fn is_ready(task: &DispatchTask, all_tasks: &[DispatchTask]) -> bool {
    task.depends_on.iter().all(|dep_label| {
        all_tasks
            .iter()
            .find(|t| &t.label == dep_label)
            .map(|t| t.status.is_terminal())
            .unwrap_or(false)
    })
}

/// Build the prompt actually delivered to the sub-agent:
/// `<parent_goal>` + `<prerequisites>` (results of dependsOn tasks) +
/// `<other_tasks>` (situational awareness of siblings) + the original prompt.
/// Mirrors TS `startTask` context construction verbatim.
pub(crate) fn build_augmented_prompt(parent: &DispatchParent, task: &DispatchTask) -> String {
    let mut ctx = format!("<parent_goal>{}</parent_goal>", parent.goal);

    if !task.depends_on.is_empty() {
        ctx.push_str("\n\n<prerequisites>");
        for dep_label in &task.depends_on {
            let Some(dep) = parent.tasks.iter().find(|t| &t.label == dep_label) else {
                continue;
            };
            ctx.push_str(&format!(
                "\n  <task label=\"{}\" agent=\"{}\" status=\"{}\">",
                dep.label,
                dep.agent_id,
                dep.status.label()
            ));
            ctx.push_str(&format!("\n    <prompt>{}</prompt>", dep.prompt));
            match dep.status {
                DispatchTaskStatus::Done => {
                    let result = match &dep.result {
                        Some(r) if !r.is_empty() => format!("\n    <result>{r}</result>"),
                        _ => "\n    <result>(task completed but produced no text output — the agent may have only used tools; check workspace for artifacts)</result>".into(),
                    };
                    ctx.push_str(&result);
                }
                // Continue-on-error: an errored/timed-out dependency still
                // unblocks this task. Inject whatever partial output it left
                // behind so the dependant can salvage it instead of silently
                // redoing the work from scratch.
                DispatchTaskStatus::Error | DispatchTaskStatus::Timeout => {
                    match &dep.result {
                        Some(r) if !r.is_empty() => ctx.push_str(&format!(
                            "\n    <result status=\"{}\">This prerequisite did NOT complete successfully — treat its output below as unverified/partial and assess it critically before relying on it:\n{r}</result>",
                            dep.status.label()
                        )),
                        _ => ctx.push_str(&format!(
                            "\n    <result status=\"{}\">(prerequisite failed and produced no output — proceed with your task using your own tools and note the gap in your answer)</result>",
                            dep.status.label()
                        )),
                    }
                }
                _ => {}
            }
            ctx.push_str("\n  </task>");
        }
        ctx.push_str("\n</prerequisites>");
    }

    let others: Vec<&DispatchTask> = parent
        .tasks
        .iter()
        .filter(|t| t.id != task.id && !task.depends_on.contains(&t.label))
        .collect();
    if !others.is_empty() {
        ctx.push_str("\n\n<other_tasks>");
        for o in others {
            if o.status == DispatchTaskStatus::Done {
                let result_tag = match &o.result {
                    Some(r) if !r.is_empty() => format!("\n    <result>{r}</result>"),
                    _ => "\n    <result>(completed, no text output)</result>".into(),
                };
                ctx.push_str(&format!(
                    "\n  <task label=\"{}\" agent=\"{}\" status=\"done\">{}{}\n  </task>",
                    o.label, o.agent_id, o.prompt, result_tag
                ));
            } else {
                ctx.push_str(&format!(
                    "\n  <task label=\"{}\" agent=\"{}\" status=\"{}\">{}</task>",
                    o.label,
                    o.agent_id,
                    o.status.label(),
                    o.prompt
                ));
            }
        }
        ctx.push_str("\n</other_tasks>");
    }

    format!("{ctx}\n\n{}", task.prompt)
}
