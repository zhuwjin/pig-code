use super::*;

use std::future::Future;

use futures_util::FutureExt as _;

use super::lifecycle::ChildLifecycle;

/// UI row a delegated child owns: the synthetic Agent tool card the client
/// renders, closed when the drive settles.
pub(super) struct DelegatedUi {
    pub(super) item_id: String,
}

/// Spawn one background child through the same managed driver as foreground
/// children (global slot, writer claim, panic/cancel settlement, one
/// completion notification). The parent turn is not blocked.
pub(super) fn spawn_detached_child(
    mut child: Session,
    task_id: String,
    cancel: CancellationToken,
    tx: async_channel::Sender<Event>,
    ui: Option<DelegatedUi>,
) {
    let Some(writer) = child
        .subagent
        .as_mut()
        .expect("child session")
        .take_writer()
    else {
        // A drive without a claim cannot write safely; settle the registered
        // task instead of leaving it Running forever.
        settle_unclaimed(&child, &task_id);
        return;
    };
    spawn_with_writer(child, writer, task_id, cancel, tx, ui);
}

fn settle_unclaimed(child: &Session, task_id: &str) {
    if let Some(entry) = child
        .state
        .tasks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter_mut()
        .find(|entry| entry.id == task_id)
        && matches!(
            entry.status,
            pig_protocol::TaskStatus::Running | pig_protocol::TaskStatus::Queued
        )
    {
        entry.status = pig_protocol::TaskStatus::Failed;
        entry.exit_code = Some(-1);
        entry.reason = Some("Subagent writer was unavailable".to_string());
        entry.ended_at = Some(crate::rollout::now_secs());
    }
    child.state.notify_terminal(task_id);
}

fn spawn_with_writer(
    mut child: Session,
    writer: crate::task::AgentWriterGuard,
    task_id: String,
    cancel: CancellationToken,
    tx: async_channel::Sender<Event>,
    ui: Option<DelegatedUi>,
) {
    // Build the lifecycle before spawning: an unpolled/dropped task still
    // releases the writer and settles the registry entry via Drop.
    let life = ChildLifecycle::new(
        &child.state,
        &child.pending,
        child.event_session_id(),
        tx.clone(),
        &child,
        writer,
        Some(task_id),
    );
    let session_id = child.event_session_id().to_string();
    let child_cancel = cancel.clone();
    let child_tx = tx.clone();
    let child_task: std::pin::Pin<
        Box<dyn std::future::Future<Output = SubagentResult> + Send + 'static>,
    > = Box::pin(async move { child.run_subagent_task(&child_tx, &child_cancel).await });
    tokio::spawn(async move {
        let managed = drive_managed(
            life,
            &cancel,
            async {
                crate::task::acquire_subagent_slot(&cancel)
                    .await
                    .map(SubagentSlot::Global)
            },
            child_task,
        )
        .await;
        if let Some(ui) = ui {
            let result = &managed.result;
            crate::session::emit_bg(&session_id, &tx, |session_id| Event::ToolCallEnd {
                session_id,
                item_id: ui.item_id,
                output: result.text().to_string(),
                is_error: result.is_error(),
                stopped: result.is_cancelled(),
                edit: None,
            });
        }
    });
}

/// Global concurrency guard held for the lifetime of one managed child. Tests
/// inject their own semaphore permit so queue/cancel behavior is deterministic.
/// The payload is never read: the slot is a Drop guard.
#[allow(dead_code)]
pub(super) enum SubagentSlot {
    Global(tokio::sync::SemaphorePermit<'static>),
    /// Test double for a child that owns no real global slot.
    #[cfg(test)]
    Test,
    /// Test permit from an independent semaphore (queue-concurrency tests).
    #[cfg(test)]
    TestPermit(tokio::sync::OwnedSemaphorePermit),
}

/// Managed child result plus whether it waited for a global slot.
pub(super) struct ManagedOutcome {
    pub(super) result: SubagentResult,
    pub(super) queued: bool,
}

/// Identity of one foreground swarm child, used when a spawned join fails so
/// the item still appears in the aggregate instead of disappearing.
#[derive(Clone)]
pub(super) struct SwarmChildIdentity {
    pub(super) description: String,
    pub(super) agent_id: String,
    pub(super) result_path: PathBuf,
}

/// One driver for every child kind: queue on the global slot (cancel-aware),
/// run under panic capture, then settle the writer, registry, approvals, panel
/// and notification exactly once. Panics inside this wrapper still settle via
/// `ChildLifecycle::drop`.
pub(super) async fn drive_managed<F>(
    life: ChildLifecycle,
    cancel: &CancellationToken,
    acquire: impl Future<Output = Option<SubagentSlot>>,
    run: F,
) -> ManagedOutcome
where
    F: Future<Output = SubagentResult>,
{
    let agent_id = life.agent_id.clone();
    let run_id = life.run_id.clone();
    match std::panic::AssertUnwindSafe(drive_managed_inner(life, cancel, acquire, run))
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

async fn drive_managed_inner<F>(
    mut life: ChildLifecycle,
    _cancel: &CancellationToken,
    acquire: impl Future<Output = Option<SubagentSlot>>,
    run: F,
) -> ManagedOutcome
where
    F: Future<Output = SubagentResult>,
{
    let queued = life.task_id.is_some() && crate::task::subagent_slots_available() == 0;
    if queued && let Some(task_id) = &life.task_id {
        crate::task::note_output(
            &life.state.tasks,
            task_id,
            "Queued: the global subagent concurrency limit is reached; waiting for a free slot…\n",
        );
    }
    let Some(_slot) = acquire.await else {
        let result = life.identify_result(cancelled_subagent_result());
        life.settle(&result);
        return ManagedOutcome { result, queued };
    };
    if let Some(task_id) = &life.task_id {
        crate::task::mark_agent_task_started(&life.state, task_id);
    }
    let result = match std::panic::AssertUnwindSafe(run).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => panic_subagent_result(payload),
    };
    let result = life.identify_result(result);
    life.settle(&result);
    ManagedOutcome { result, queued }
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

/// Model-facing single-Agent receipt. Completion, failure and max-turn
/// exhaustion keep the agent_id and a resume path; the parent can always
/// continue the same contextual child.
pub(super) fn subagent_receipt(agent_id: &str, profile: &str, result: &SubagentResult) -> String {
    let succeeded = result.is_completed() && !result.is_error();
    let status = if succeeded { "completed" } else { "failed" };
    let heading = if succeeded { "[summary]" } else { "[failure]" };
    let cwd = result
        .artifact
        .as_ref()
        .and_then(|artifact| artifact.cwd.as_ref())
        .map(|cwd| format!("\ncwd: {}", cwd.display()))
        .unwrap_or_default();
    format!(
        "agent_id: {agent_id}\nsubagent_type: {profile}\nstatus: {status}\nturns: {}\n{heading}\n{}\nresume_hint: continue this subagent with Agent(resume=\"{agent_id}\", prompt=\"...\"){cwd}",
        result.turns,
        result.text().to_string()
    )
}

pub(super) fn swarm_child_result(
    identity: Option<&SwarmChildIdentity>,
    managed: ManagedOutcome,
) -> crate::agent::SwarmChildResult {
    let result = managed.result;
    let status = if result.is_cancelled() {
        crate::agent::SwarmChildStatus::Cancelled
    } else if result.is_error() || !result.is_completed() {
        crate::agent::SwarmChildStatus::Failed
    } else {
        crate::agent::SwarmChildStatus::Completed
    };
    let result_path = identity.and_then(|identity| {
        (!result.is_cancelled() && std::fs::metadata(&identity.result_path).is_ok())
            .then(|| identity.result_path.clone())
    });
    crate::agent::SwarmChildResult {
        description: identity
            .map(|identity| identity.description.clone())
            .unwrap_or_else(|| "swarm subtask".to_string()),
        agent_id: identity.map(|identity| identity.agent_id.clone()),
        status,
        turns: result.turns,
        result_path,
        result_text: format!(
            "{}{}",
            result.text(),
            result
                .artifact
                .as_ref()
                .and_then(|artifact| artifact.cwd.as_ref())
                .map(|cwd| format!("\ncwd: {}", cwd.display()))
                .unwrap_or_default()
        ),
        queued: managed.queued,
        usage: result.usage,
    }
}

pub(super) fn aborted_swarm_child(
    identity: &SwarmChildIdentity,
    error: String,
) -> crate::agent::SwarmChildResult {
    crate::agent::SwarmChildResult {
        description: identity.description.clone(),
        agent_id: Some(identity.agent_id.clone()),
        status: crate::agent::SwarmChildStatus::Failed,
        turns: 0,
        result_path: None,
        result_text: error,
        queued: false,
        usage: (0, 0, 0),
    }
}
