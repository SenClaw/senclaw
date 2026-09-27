//! Implementation of DispatchBridgeApi for DispatchBridge.

use super::bridge::DispatchBridge;
use super::traits::DispatchBridgeApi;
use super::types::{AdminActivityCallback, DispatchParent, DispatchTaskStatus};

impl DispatchBridgeApi for DispatchBridge {
    fn notify_task_done(&self, task_id: &str, content: &str) {
        self.mark_task_done(task_id, content);
    }

    fn notify_reply(&self, agent_jid: &str, content: &str) {
        if let Some(task_id) = self.earliest_processing_for_jid(agent_jid) {
            self.mark_task_done(&task_id, content);
        }
    }

    fn notify_error(&self, agent_jid: &str, error: &str) {
        if let Some(task_id) = self.earliest_processing_for_jid(agent_jid) {
            self.mark_task_error(&task_id, error);
        }
    }

    fn get_parents(&self) -> Vec<DispatchParent> {
        self.read_state().map(|s| s.parents).unwrap_or_default()
    }

    fn set_admin_activity_callback(&self, cb: AdminActivityCallback) {
        *self.on_admin_activity.lock().unwrap() = Some(cb);
    }

    fn has_active_dispatch(&self, folder: &str) -> bool {
        self.read_state()
            .map(|s| {
                s.parents.iter().any(|p| {
                    p.admin_folder == folder && (p.status == "active" || p.status == "queued")
                })
            })
            .unwrap_or(false)
    }

    fn pause_admin(&self, folder: &str) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .paused_admins
            .insert(folder.to_string());
        let mut child_jids: Vec<String> = Vec::new();
        if let Ok(state) = self.read_state() {
            let active = self.inner.lock().unwrap().active_tasks.clone();
            for parent in &state.parents {
                if parent.admin_folder != folder || parent.status != "active" {
                    continue;
                }
                for task in &parent.tasks {
                    if task.status == DispatchTaskStatus::Processing
                        && !task.agent_jid.is_empty()
                        && active.contains_key(&task.id)
                    {
                        child_jids.push(task.agent_jid.clone());
                    }
                }
            }
        }
        tracing::info!(
            "[DispatchBridge] pauseAdmin({folder}): blocked scheduling, child jids: [{}]",
            child_jids.join(", ")
        );
        child_jids
    }

    fn resume_admin(&self, folder: &str) {
        self.inner.lock().unwrap().paused_admins.remove(folder);
        tracing::info!("[DispatchBridge] resumeAdmin({folder}): scheduling unblocked");
    }

    fn cancel_admin_parents(&self, folder: &str) -> Vec<String> {
        self.cancel_active_parents_where(
            |p| p.admin_folder == folder,
            "Cancelled: admin agent stopped",
        )
    }

    fn set_task_file_changes(&self, task_id: &str, file_changes: Vec<super::types::FileChange>) {
        self.set_task_file_changes(task_id, file_changes);
    }

    fn add_file_change(&self, task_id: &str, path: &str, change_type: &str) {
        self.add_file_change(task_id, path, change_type);
    }
}
