use super::*;

use std::future::Future;

use futures_util::FutureExt as _;

use super::drive::drive_subagent;
use super::lifecycle::ChildLifecycle;
use super::state::{ProgressSink, SubagentDrive};

pub(super) struct DetachedGateCtx {
    pub(super) cwd: PathBuf,
    pub(super) permissions: PermissionSettings,
    pub(super) hooks: crate::hooks::HookSet,
    pub(super) permission_rules: crate::permissions::PermissionRules,
    pub(super) always_allowed: HashSet<(String, String)>,
    pub(super) state: crate::task::SessionToolState,
    /// Child-local tool state: shared task/steer/writer handles, own TodoHandle.
    pub(super) tool_state: crate::task::SessionToolState,
    pub(super) pending: PendingApprovals,
    pub(super) store: Arc<Mutex<Store>>,
    pub(super) session_id: String,
    pub(super) history: Vec<ChatMsg>,
    /// Detached child tracker: shared session baselines/dirty queue, isolated
    /// turn summary.
    pub(super) tracker: ChangeTracker,
}

/// UI row a delegated child owns: the synthetic Agent tool card the client
/// renders, closed when the drive settles.
pub(super) struct DelegatedUi {
    pub(super) item_id: String,
}

/// Spawn one background child through the same managed driver as foreground
/// children (global slot, writer claim, panic/cancel settlement, one
/// completion notification). The parent turn is not blocked.
pub(super) fn spawn_detached_child(
    ctx: DetachedGateCtx,
    mut drive: SubagentDrive,
    task_id: String,
    cancel: CancellationToken,
    tx: async_channel::Sender<Event>,
    ui: Option<DelegatedUi>,
) {
    let progress = ProgressSink::Background {
        task_id: task_id.clone(),
    };
    let Some(writer) = drive.take_writer() else {
        // Defensive: a drive without a claim cannot write safely; settle the
        // registered task instead of leaving it Running forever.
        if let Some(entry) = ctx
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
        ctx.state.notify_terminal(&task_id);
        return;
    };
    // Build the lifecycle before spawning: an unpolled/dropped task still
    // releases the writer and settles the registry entry via Drop.
    let life = ChildLifecycle::new(
        &ctx.state,
        &ctx.pending,
        &ctx.session_id,
        tx.clone(),
        &drive,
        writer,
        &progress,
        Some(task_id),
    );
    tokio::spawn(async move {
        let mut ctx = ctx;
        let mut drive = drive;
        let mut tracker = std::mem::take(&mut ctx.tracker);
        let managed = drive_managed(
            life,
            &cancel,
            async {
                crate::task::acquire_subagent_slot(&cancel)
                    .await
                    .map(SubagentSlot::Global)
            },
            async {
                let mcp_extra = drive.extra_tools();
                let mut gate = GateCtx {
                    cwd: &ctx.cwd,
                    permissions: ctx.permissions,
                    media: tool::MediaCapabilities::new(
                        drive.child_config.input_image,
                        drive.child_config.input_pdf,
                    ),
                    tracker: &mut tracker,
                    state: &ctx.tool_state,
                    owns_todo_state: false,
                    pending: &ctx.pending,
                    permission_rules: &mut ctx.permission_rules,
                    always_allowed: &mut ctx.always_allowed,
                    session_id: &ctx.session_id,
                    plan_enabled: false,
                    store: &ctx.store,
                    extra_tools: &mcp_extra,
                    hooks: &ctx.hooks,
                    history: &ctx.history,
                };
                drive_subagent(&mut gate, &mut drive, &progress, &tx, &cancel).await
            },
        )
        .await;
        if let Some(ui) = ui {
            let result = &managed.result;
            crate::session::emit_bg(&ctx.session_id, &tx, |session_id| Event::ToolCallEnd {
                session_id,
                item_id: ui.item_id,
                output: result.result_text.clone(),
                is_error: result.is_error,
                stopped: result.cancelled,
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
    pub(super) result: SubagentDriveResult,
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
    F: Future<Output = SubagentDriveResult>,
{
    match std::panic::AssertUnwindSafe(drive_managed_inner(life, cancel, acquire, run))
        .catch_unwind()
        .await
    {
        Ok(outcome) => outcome,
        Err(payload) => ManagedOutcome {
            result: panic_drive_result(payload),
            queued: false,
        },
    }
}

async fn drive_managed_inner<F>(
    mut life: ChildLifecycle,
    _cancel: &CancellationToken,
    acquire: impl Future<Output = Option<SubagentSlot>>,
    run: F,
) -> ManagedOutcome
where
    F: Future<Output = SubagentDriveResult>,
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
        let result = cancelled_drive_result();
        life.settle(&result);
        return ManagedOutcome { result, queued };
    };
    if let Some(task_id) = &life.task_id {
        crate::task::mark_agent_task_started(&life.state, task_id);
    }
    let result = match std::panic::AssertUnwindSafe(run).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => panic_drive_result(payload),
    };
    life.settle(&result);
    ManagedOutcome { result, queued }
}

pub(super) fn cancelled_drive_result() -> SubagentDriveResult {
    SubagentDriveResult {
        status_line: "cancelled".to_string(),
        result_text: String::new(),
        turns: 0,
        completed: false,
        is_error: false,
        usage: (0, 0, 0),
        cancelled: true,
    }
}

pub(super) fn panic_drive_result(payload: Box<dyn std::any::Any + Send>) -> SubagentDriveResult {
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| message.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    let note = format!("Subagent driver panicked: {message}");
    SubagentDriveResult {
        status_line: format!("failed: {note}"),
        result_text: note,
        turns: 0,
        completed: false,
        is_error: true,
        usage: (0, 0, 0),
        cancelled: false,
    }
}

/// Model-facing single-Agent receipt. Completion, failure and max-turn
/// exhaustion keep the agent_id and a resume path; the parent can always
/// continue the same contextual child.
pub(super) fn subagent_receipt(
    agent_id: &str,
    profile: &str,
    result: &SubagentDriveResult,
) -> String {
    let succeeded = result.completed && !result.is_error;
    let status = if succeeded { "completed" } else { "failed" };
    let heading = if succeeded { "[summary]" } else { "[failure]" };
    format!(
        "agent_id: {agent_id}\nsubagent_type: {profile}\nstatus: {status}\nturns: {}\n{heading}\n{}\nresume_hint: continue this subagent with Agent(resume=\"{agent_id}\", prompt=\"...\")",
        result.turns, result.result_text
    )
}

pub(super) fn swarm_child_result(
    identity: Option<&SwarmChildIdentity>,
    managed: ManagedOutcome,
) -> crate::agent::SwarmChildResult {
    let result = managed.result;
    let status = if result.cancelled {
        crate::agent::SwarmChildStatus::Cancelled
    } else if result.is_error || !result.completed {
        crate::agent::SwarmChildStatus::Failed
    } else {
        crate::agent::SwarmChildStatus::Completed
    };
    let result_path = identity.and_then(|identity| {
        (!result.cancelled && std::fs::metadata(&identity.result_path).is_ok())
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
        result_text: result.result_text,
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
