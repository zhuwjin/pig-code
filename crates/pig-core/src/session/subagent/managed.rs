use super::*;

/// UI row a delegated child owns: the synthetic Agent tool card the client
/// renders, closed when the drive settles.
pub(super) struct DelegatedUi {
    pub(super) item_id: String,
}

/// Spawn one background child through the same managed driver as foreground
/// children (session-tree slot, writer claim, panic/cancel settlement, one
/// completion notification). The parent turn is not blocked.
pub(super) fn spawn_detached_child(
    mut child: Session,
    task_id: String,
    cancel: CancellationToken,
    tx: async_channel::Sender<Event>,
    ui: Option<DelegatedUi>,
    activation: crate::agent::AgentExecutionActivation,
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
    spawn_with_writer(child, writer, task_id, cancel, tx, ui, activation);
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
    if let Some(run_id) = child.agent.run_id() {
        child.state.agents.settle_run(
            &child.agent.id,
            &run_id,
            crate::session::AgentRunStatus::Failed,
        );
    }
    crate::task::release_agent_mailbox(&child.state, &child.agent.id);
}

fn spawn_with_writer(
    child: Session,
    writer: crate::agent::AgentWriterGuard,
    task_id: String,
    cancel: CancellationToken,
    tx: async_channel::Sender<Event>,
    ui: Option<DelegatedUi>,
    activation: crate::agent::AgentExecutionActivation,
) {
    // Build the lifecycle before spawning: an unpolled/dropped task still
    // releases the writer and settles the registry entry via Drop.
    let life = AgentRunSupervisor::new(
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
    let backends = child.state.backends.clone();
    tokio::spawn(async move {
        let managed =
            crate::agent::execute_local(backends, child, child_tx, child_cancel, activation, life)
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

#[cfg(test)]
pub(super) type SubagentSlot = crate::agent::AgentRunPermit;

/// Identity of one foreground swarm child, used when a spawned join fails so
/// the item still appears in the aggregate instead of disappearing.
#[derive(Clone)]
pub(super) struct SwarmChildIdentity {
    pub(super) description: String,
    pub(super) agent_id: String,
    pub(super) result_path: PathBuf,
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
