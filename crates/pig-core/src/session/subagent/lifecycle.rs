use super::*;

use crate::task::AgentWriterGuard;

use super::drive::{agent_result_path, unexpected_drive_result};
use super::state::{ProgressSink, SubagentDrive, agent_card_model};

/// One child's terminal lifecycle: exclusive JSONL writer, registry entry,
/// live panel indicator, approvals and (for background children) the one
/// completion notification. Drop is the backstop for panics: the writer is
/// released, approvals are dropped, a Running registry entry becomes a
/// failure, and the panel indicator is cleared.
pub(super) struct ChildLifecycle {
    pub(super) state: crate::task::SessionToolState,
    pub(super) pending: PendingApprovals,
    pub(super) session_id: String,
    pub(super) tx: async_channel::Sender<Event>,
    pub(super) task_id: Option<String>,
    pub(super) agent_id: String,
    pub(super) ownership: String,
    pub(super) writer: Option<AgentWriterGuard>,
    pub(super) finished_emitted: bool,
    pub(super) registry_settled: bool,
    pub(super) approvals_released: bool,
    pub(super) settled: bool,
}

impl ChildLifecycle {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        state: &crate::task::SessionToolState,
        pending: &PendingApprovals,
        session_id: &str,
        tx: async_channel::Sender<Event>,
        drive: &SubagentDrive,
        writer: AgentWriterGuard,
        progress: &ProgressSink,
        task_id: Option<String>,
    ) -> Self {
        if let Some(task_id) = &task_id {
            let mut tasks = state
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entry) = tasks.iter_mut().find(|entry| &entry.id == task_id) {
                entry.record_path = Some(drive.jsonl.clone());
                entry.result_path = Some(agent_result_path(&drive.jsonl, &drive.agent_id));
                entry.model = Some(agent_card_model(drive));
            }
        }
        Self {
            state: state.clone(),
            pending: pending.clone(),
            session_id: session_id.to_string(),
            tx,
            task_id,
            agent_id: drive.agent_id.clone(),
            ownership: progress.ownership_prefix(&drive.agent_id),
            writer: Some(writer),
            finished_emitted: false,
            registry_settled: false,
            approvals_released: false,
            settled: false,
        }
    }

    fn release_writer(&mut self) {
        if let Some(mut writer) = self.writer.take() {
            writer.release();
        }
    }

    /// Remove this child's pending approvals so a terminated driver can never
    /// receive a late grant. Request ids start with `{session_id}-{ownership}-`
    /// and carry an `-approval-` segment; the ownership is unique per child
    /// (the foreground swarm ownership includes the agent_id).
    fn release_approvals(&mut self) {
        if self.approvals_released {
            return;
        }
        self.approvals_released = true;
        let prefix = format!("{}-{}-", self.session_id, self.ownership);
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|request_id, _| {
                !(request_id.starts_with(&prefix) && request_id.contains("-approval-"))
            });
    }

    fn settle_registry(&mut self, result: &SubagentDriveResult) {
        if self.registry_settled {
            return;
        }
        self.registry_settled = true;
        let Some(task_id) = &self.task_id else {
            return;
        };
        let mut tasks = self
            .state
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = tasks.iter_mut().find(|entry| &entry.id == task_id)
            && matches!(
                entry.status,
                pig_protocol::TaskStatus::Running | pig_protocol::TaskStatus::Queued
            )
        {
            entry.status = if result.cancelled {
                pig_protocol::TaskStatus::Killed
            } else if result.is_error || !result.completed {
                pig_protocol::TaskStatus::Failed
            } else {
                pig_protocol::TaskStatus::Completed
            };
            entry.exit_code = if result.cancelled {
                None
            } else if result.is_error || !result.completed {
                Some(-1)
            } else {
                Some(0)
            };
            entry.reason = (result.is_error || !result.completed || result.cancelled)
                .then(|| result.result_text.clone());
            entry.turns = Some(u32::try_from(result.turns).unwrap_or(u32::MAX));
            entry.ended_at = Some(crate::rollout::now_secs());
        }
    }

    fn note_output(&self, result: &SubagentDriveResult) {
        if let Some(task_id) = &self.task_id {
            crate::task::note_output(
                &self.state.tasks,
                task_id,
                &format!("[{}]\n{}\n", result.status_line, result.result_text),
            );
        }
    }

    fn emit_finished(&mut self) {
        if self.finished_emitted {
            return;
        }
        self.finished_emitted = true;
        let agent_id = self.agent_id.clone();
        emit_bg(&self.session_id, &self.tx, |session_id| {
            Event::SubagentActivity {
                session_id,
                agent_id,
                item: None,
                finished: true,
            }
        });
    }

    fn task_changed(&self) {
        if let Some(task_id) = &self.task_id {
            self.state.notify_terminal(task_id);
        }
    }

    pub(super) fn settle(&mut self, result: &SubagentDriveResult) {
        self.release_writer();
        self.release_approvals();
        self.settle_registry(result);
        self.note_output(result);
        self.emit_finished();
        self.task_changed();
        self.settled = true;
    }
}

impl Drop for ChildLifecycle {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.release_writer();
        self.release_approvals();
        let result = unexpected_drive_result();
        self.settle_registry(&result);
        self.emit_finished();
        self.task_changed();
    }
}
