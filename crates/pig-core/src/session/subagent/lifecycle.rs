use super::*;

use crate::task::AgentWriterGuard;

use super::state::agent_card_model;
use super::turn::{agent_result_path, unexpected_subagent_result};

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
    pub(super) run_id: Option<String>,
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
        child: &Session,
        writer: AgentWriterGuard,
        task_id: Option<String>,
    ) -> Self {
        if let Some(task_id) = &task_id {
            let mut tasks = state
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entry) = tasks.iter_mut().find(|entry| &entry.id == task_id) {
                entry.record_path = Some(
                    child
                        .subagent
                        .as_ref()
                        .expect("child session")
                        .jsonl
                        .clone(),
                );
                entry.result_path = Some(agent_result_path(
                    &child.subagent.as_ref().expect("child session").jsonl,
                    &child.agent.id,
                ));
                entry.model = Some(agent_card_model(child));
            }
        }
        Self {
            state: state.clone(),
            pending: pending.clone(),
            session_id: session_id.to_string(),
            tx,
            task_id,
            agent_id: child.agent.id.clone(),
            run_id: child.agent.run_id(),
            ownership: child
                .subagent
                .as_ref()
                .expect("child session")
                .progress
                .ownership_prefix(&child.agent.id),
            writer: Some(writer),
            finished_emitted: false,
            registry_settled: false,
            approvals_released: false,
            settled: false,
        }
    }

    pub(super) fn identify_result(&self, result: SubagentResult) -> SubagentResult {
        match &self.run_id {
            Some(run_id) => result.with_identity(&self.agent_id, run_id.clone()),
            None => result,
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

    fn settle_registry(&mut self, result: &SubagentResult) {
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
            entry.status = if result.is_cancelled() {
                pig_protocol::TaskStatus::Killed
            } else if result.is_error() || !result.is_completed() {
                pig_protocol::TaskStatus::Failed
            } else {
                pig_protocol::TaskStatus::Completed
            };
            entry.exit_code = if result.is_cancelled() {
                None
            } else if result.is_error() || !result.is_completed() {
                Some(-1)
            } else {
                Some(0)
            };
            entry.reason = (result.is_error() || !result.is_completed() || result.is_cancelled())
                .then(|| result.text().to_string());
            entry.turns = Some(u32::try_from(result.turns).unwrap_or(u32::MAX));
            if let Some(run) = entry.run.as_mut() {
                run.usage = result.usage;
            }
            entry.ended_at = Some(crate::rollout::now_secs());
        }
    }

    fn note_output(&self, result: &SubagentResult) {
        if let Some(task_id) = &self.task_id {
            crate::task::note_output(
                &self.state.tasks,
                task_id,
                &format!("[{}]\n{}\n", result.status_line(), result.text()),
            );
        }
    }

    fn emit_finished(&mut self) {
        if self.finished_emitted {
            return;
        }
        self.finished_emitted = true;
        let agent_id = self.agent_id.clone();
        let run_id = self.run_id.clone();
        emit_bg(&self.session_id, &self.tx, |session_id| {
            Event::ConversationStatus {
                session_id,
                agent_id,
                running: false,
                run_id,
            }
        });
    }

    fn task_changed(&self) {
        if let Some(task_id) = &self.task_id {
            self.state.notify_terminal(task_id);
        }
    }

    pub(super) fn settle(&mut self, result: &SubagentResult) {
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
        let result = unexpected_subagent_result();
        self.settle_registry(&result);
        self.emit_finished();
        self.task_changed();
    }
}
