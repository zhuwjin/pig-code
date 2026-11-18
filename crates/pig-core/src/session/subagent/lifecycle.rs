use super::*;

use std::future::Future;

use futures_util::{FutureExt as _, future::BoxFuture};

use crate::agent::AgentWriterGuard;

use super::state::agent_card_model;
use super::turn::{agent_result_path, unexpected_subagent_result};

#[cfg(not(test))]
const LOCAL_RUN_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const LOCAL_RUN_STOP_GRACE: std::time::Duration = std::time::Duration::from_millis(50);

pub(crate) struct ManagedOutcome {
    pub(crate) result: SubagentResult,
    pub(crate) queued: bool,
}

/// One child's terminal lifecycle: exclusive JSONL writer, registry entry,
/// live panel indicator, approvals and (for background children) the one
/// completion notification. Drop is the backstop for panics: the writer is
/// released, approvals are dropped, a Running registry entry becomes a
/// failure, and the panel indicator is cleared.
pub struct AgentRunSupervisor {
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

impl AgentRunSupervisor {
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
        if let Some(task_id) = &task_id {
            state.notify_task(task_id);
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
        if let Some(run_id) = self.run_id.as_deref() {
            self.state.agents.settle_run_with_outcome(
                &self.agent_id,
                run_id,
                result.status,
                Some(result.stop_reason.clone()),
                result.usage,
                result.turns,
            );
        }
        crate::task::release_agent_mailbox(&self.state, &self.agent_id);
        self.emit_finished();
        self.task_changed();
        self.settled = true;
    }

    pub(super) async fn drive<F>(
        self,
        cancel: &CancellationToken,
        acquire: impl Future<Output = Option<crate::agent::AgentRunPermit>>,
        run: F,
    ) -> ManagedOutcome
    where
        F: Future<Output = SubagentResult>,
    {
        let agent_id = self.agent_id.clone();
        let run_id = self.run_id.clone();
        match std::panic::AssertUnwindSafe(self.drive_inner(cancel, acquire, run))
            .catch_unwind()
            .await
        {
            Ok(outcome) => outcome,
            Err(payload) => {
                let result = panic_subagent_result(payload);
                ManagedOutcome {
                    result: match run_id {
                        Some(run_id) => result.with_identity(&agent_id, run_id),
                        None => result,
                    },
                    queued: false,
                }
            }
        }
    }

    async fn drive_inner<F>(
        mut self,
        cancel: &CancellationToken,
        acquire: impl Future<Output = Option<crate::agent::AgentRunPermit>>,
        run: F,
    ) -> ManagedOutcome
    where
        F: Future<Output = SubagentResult>,
    {
        if let Some(run_id) = &self.run_id
            && let Err(error) = self
                .state
                .agents
                .register_run_cancel(run_id.clone(), cancel.clone())
        {
            tracing::warn!(%run_id, %error, "failed to register child run cancellation");
        }
        let queued = self.task_id.is_some() && self.state.scheduler.available() == 0;
        if queued && let Some(task_id) = &self.task_id {
            crate::task::note_output(
                &self.state.tasks,
                task_id,
                "Queued: the session-tree subagent concurrency limit is reached; waiting for a free slot…\n",
            );
        }
        let Some(_slot) = acquire.await else {
            let result = self.identify_result(cancelled_subagent_result());
            self.settle(&result);
            return ManagedOutcome { result, queued };
        };
        if let Some(task_id) = &self.task_id {
            crate::task::mark_agent_task_started(&self.state, task_id);
        }
        let result = {
            let execution = std::panic::AssertUnwindSafe(run).catch_unwind();
            tokio::pin!(execution);
            let outcome = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tokio::time::timeout(LOCAL_RUN_STOP_GRACE, &mut execution).await.ok()
                }
                result = &mut execution => Some(result),
            };
            let result = match outcome {
                Some(Ok(result)) => result,
                Some(Err(payload)) => panic_subagent_result(payload),
                None => forced_stop_subagent_result(),
            };
            self.identify_result(result)
        };
        self.settle(&result);
        ManagedOutcome { result, queued }
    }
}

impl Drop for AgentRunSupervisor {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.release_writer();
        self.release_approvals();
        let result = unexpected_subagent_result();
        self.settle_registry(&result);
        if let Some(run_id) = self.run_id.as_deref() {
            self.state.agents.settle_run_with_outcome(
                &self.agent_id,
                run_id,
                result.status,
                Some(result.stop_reason.clone()),
                result.usage,
                result.turns,
            );
        }
        crate::task::release_agent_mailbox(&self.state, &self.agent_id);
        self.emit_finished();
        self.task_changed();
    }
}

impl Session {
    pub(crate) fn execution_runtime(&self) -> Option<crate::agent::runtime::AgentRuntime> {
        self.state.agents.execution_runtime()
    }

    pub(crate) fn begin_agent_execution(&self, agent_id: &str, run_id: String) -> bool {
        self.state.agents.begin_run(agent_id, run_id)
    }
}

pub(crate) struct LocalAgentExecutionHandle {
    identity: crate::agent::AgentExecutionIdentity,
    state: crate::task::SessionToolState,
    cancel: CancellationToken,
    events: async_channel::Receiver<Event>,
    drive: Option<BoxFuture<'static, ManagedOutcome>>,
    cursor: crate::agent::AgentExecutionCursor,
    terminal: Option<(
        crate::agent::AgentExecutionCursor,
        crate::agent::AgentBackendResult,
    )>,
    released: Option<crate::agent::AgentExecutionRelease>,
    stop_requested: bool,
}

impl LocalAgentExecutionHandle {
    pub(crate) fn new(
        mut session: Session,
        supervisor: AgentRunSupervisor,
        mut request: crate::agent::AgentExecutionRequest,
    ) -> Result<Self, crate::agent::AgentBackendError> {
        let identity = request.execution.clone();
        if request.identity != identity.run
            || identity.run.agent_id != session.agent.id
            || session.agent.run_id().as_deref() != Some(identity.run.run_id.as_str())
        {
            return Err(crate::agent::AgentBackendError::InvalidContext(
                "Local execution identity mismatch".to_string(),
            ));
        }
        if request.activation == crate::agent::AgentExecutionActivation::Reconnect {
            return Err(crate::agent::AgentBackendError::UnsupportedCapability {
                backend_id: "local".to_string(),
                capability: crate::agent::AgentExecutionCapability::Reconnect,
            });
        }
        let state = session.state.clone();
        let cancel = request.cancel.clone();
        let (events, output) = async_channel::unbounded();
        request.events = events.clone();
        let scheduler = state.scheduler.clone();
        let run = Box::pin(async move { session.run_subagent_task(&events, &cancel).await });
        let drive_cancel = request.cancel.clone();
        let drive = Box::pin(async move {
            supervisor
                .drive(&drive_cancel, scheduler.acquire(&drive_cancel), run)
                .await
        }) as BoxFuture<'static, ManagedOutcome>;
        let cursor = crate::agent::AgentExecutionCursor {
            attempt: identity.attempt,
            backend_epoch: identity.backend_epoch.clone(),
            sequence: 0,
        };
        Ok(Self {
            identity,
            state,
            cancel: request.cancel.clone(),
            events: output,
            drive: Some(drive),
            cursor,
            terminal: None,
            released: None,
            stop_requested: false,
        })
    }

    fn receipt(&self) -> crate::agent::AgentExecutionReceipt {
        crate::agent::AgentExecutionReceipt {
            cursor: self.cursor.clone(),
        }
    }

    fn settle(&mut self, outcome: ManagedOutcome) -> crate::agent::AgentExecutionUpdate {
        let queued = outcome.queued;
        let mut result = crate::agent::AgentBackendResult::from_run(outcome.result);
        result.queued = queued;
        self.cursor.sequence += 1;
        let cursor = self.cursor.clone();
        self.terminal = Some((cursor.clone(), result.clone()));
        self.drive = None;
        crate::agent::AgentExecutionUpdate::Settled { cursor, result }
    }

    fn capabilities(&self) -> std::collections::BTreeSet<crate::agent::AgentExecutionCapability> {
        [
            crate::agent::AgentExecutionCapability::Background,
            crate::agent::AgentExecutionCapability::Steer,
            crate::agent::AgentExecutionCapability::Resume,
            crate::agent::AgentExecutionCapability::Stop,
            crate::agent::AgentExecutionCapability::ForceStop,
            crate::agent::AgentExecutionCapability::StructuredOutput,
            crate::agent::AgentExecutionCapability::Approval,
            crate::agent::AgentExecutionCapability::Media,
            crate::agent::AgentExecutionCapability::Worktree,
        ]
        .into_iter()
        .collect()
    }
}

impl crate::agent::AgentExecutionHandle for LocalAgentExecutionHandle {
    fn identity(&self) -> &crate::agent::AgentExecutionIdentity {
        &self.identity
    }

    fn capabilities(&self) -> std::collections::BTreeSet<crate::agent::AgentExecutionCapability> {
        Self::capabilities(self)
    }

    fn send(
        &mut self,
        input: crate::agent::AgentExecutionInput,
    ) -> BoxFuture<'_, Result<crate::agent::AgentExecutionReceipt, crate::agent::AgentBackendError>>
    {
        Box::pin(async move {
            if self.terminal.is_some() || self.stop_requested {
                return Err(crate::agent::AgentBackendError::InvalidContext(
                    "Local execution is not accepting input".to_string(),
                ));
            }
            let text = input
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    crate::agent::AgentBackendError::InvalidContext(
                        "Local execution input requires a text field".to_string(),
                    )
                })?
                .to_string();
            self.state
                .agents
                .push_steer(
                    &self.identity.run.agent_id,
                    crate::agent::AgentSteer::restored(input.message_id, text, None),
                )
                .map_err(crate::agent::AgentBackendError::Failed)?;
            Ok(self.receipt())
        })
    }

    fn stop(
        &mut self,
        stop: crate::agent::AgentExecutionStop,
    ) -> BoxFuture<'_, Result<crate::agent::AgentExecutionReceipt, crate::agent::AgentBackendError>>
    {
        Box::pin(async move {
            if self.terminal.is_some() {
                return Err(crate::agent::AgentBackendError::InvalidContext(
                    "Local execution has already settled".to_string(),
                ));
            }
            if self.stop_requested {
                return Ok(self.receipt());
            }
            let reason = match stop {
                crate::agent::AgentExecutionStop::Graceful { reason }
                | crate::agent::AgentExecutionStop::Force { reason } => reason,
            };
            self.state
                .agents
                .request_run_stop(&self.identity.run.run_id, &reason)
                .map_err(crate::agent::AgentBackendError::InvalidContext)?;
            self.cancel.cancel();
            self.stop_requested = true;
            Ok(self.receipt())
        })
    }

    fn detach(
        &mut self,
    ) -> BoxFuture<'_, Result<crate::agent::AgentExecutionReceipt, crate::agent::AgentBackendError>>
    {
        Box::pin(async move {
            Err(crate::agent::AgentBackendError::UnsupportedCapability {
                backend_id: "local".to_string(),
                capability: crate::agent::AgentExecutionCapability::Detach,
            })
        })
    }

    fn next(
        &mut self,
    ) -> BoxFuture<'_, Result<crate::agent::AgentExecutionUpdate, crate::agent::AgentBackendError>>
    {
        Box::pin(async move {
            if let Some((cursor, result)) = &self.terminal {
                return Ok(crate::agent::AgentExecutionUpdate::Settled {
                    cursor: cursor.clone(),
                    result: result.clone(),
                });
            }
            let drive = self.drive.as_mut().ok_or_else(|| {
                crate::agent::AgentBackendError::InvalidContext(
                    "Local execution has no active drive".to_string(),
                )
            })?;
            tokio::select! {
                biased;
                event = self.events.recv() => {
                    match event {
                        Ok(event) => {
                            self.cursor.sequence += 1;
                            Ok(crate::agent::AgentExecutionUpdate::Output {
                                sequence: self.cursor.sequence,
                                event,
                            })
                        }
                        Err(_) => {
                            let outcome = drive.await;
                            Ok(self.settle(outcome))
                        }
                    }
                }
                outcome = &mut *drive => Ok(self.settle(outcome)),
            }
        })
    }

    fn release(
        &mut self,
    ) -> BoxFuture<'_, Result<crate::agent::AgentExecutionRelease, crate::agent::AgentBackendError>>
    {
        Box::pin(async move {
            if let Some(released) = &self.released {
                return Ok(released.clone());
            }
            while self.terminal.is_none() {
                if !self.stop_requested {
                    return Err(crate::agent::AgentBackendError::InvalidContext(
                        "Local execution must settle before release".to_string(),
                    ));
                }
                let _ = self.next().await?;
            }
            self.drive = None;
            let cursor = self
                .terminal
                .as_ref()
                .map(|(cursor, _)| cursor.clone())
                .unwrap_or_else(|| self.cursor.clone());
            self.cursor = cursor.clone();
            let released = crate::agent::AgentExecutionRelease {
                identity: self.identity.clone(),
                cursor,
                released_at_ms: crate::model_io::now_ms(),
            };
            self.released = Some(released.clone());
            Ok(released)
        })
    }
}

fn forced_stop_subagent_result() -> SubagentResult {
    SubagentResult::child(
        crate::session::AgentRunStatus::Cancelled,
        "cancellation grace period elapsed; local execution was dropped".to_string(),
        String::new(),
        0,
        (0, 0, 0),
    )
}

pub(super) fn cancelled_subagent_result() -> SubagentResult {
    SubagentResult::child(
        crate::session::AgentRunStatus::Cancelled,
        "cancelled".to_string(),
        String::new(),
        0,
        (0, 0, 0),
    )
}

pub(super) fn panic_subagent_result(payload: Box<dyn std::any::Any + Send>) -> SubagentResult {
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| message.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    let note = format!("Subagent driver panicked: {message}");
    SubagentResult::child(
        crate::session::AgentRunStatus::Failed,
        note.clone(),
        note,
        0,
        (0, 0, 0),
    )
}
