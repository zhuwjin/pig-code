//! Process-local execution resources and in-memory projections for AgentRuntime.
//! Agent identity, run state, lineage and durable mailbox state belong to AgentRuntime;
//! this module owns writer guards, execution quiescence and live delivery projections.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use pig_protocol::AgentRunStatus;

use super::{AgentMailbox, AgentSteer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentLifecycle {
    Queued,
    Running,
    Idle,
    Lost,
    Closed,
}

impl AgentLifecycle {
    pub(crate) fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentRunState {
    pub(crate) run_id: String,
    pub(crate) status: AgentRunStatus,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) depth: u32,
}

impl AgentRunState {
    pub(crate) fn new(
        run_id: String,
        status: AgentRunStatus,
        parent_agent_id: Option<String>,
        depth: u32,
    ) -> Self {
        Self {
            run_id,
            status,
            parent_agent_id,
            depth,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentPartial {
    pub(crate) run_id: String,
    pub(crate) text: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentNode {
    pub(crate) agent_id: String,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) depth: u32,
    pub(crate) lifecycle: AgentLifecycle,
    pub(crate) current_run: Option<AgentRunState>,
    pub(crate) run_history: Vec<AgentRunState>,
    pub(crate) mailbox: AgentMailbox,
    pub(crate) partial: Option<AgentPartial>,
    supervised: bool,
}

impl AgentNode {
    pub(crate) fn root(agent_id: String) -> Self {
        Self {
            agent_id,
            parent_agent_id: None,
            depth: 0,
            lifecycle: AgentLifecycle::Idle,
            current_run: None,
            run_history: Vec::new(),
            mailbox: AgentMailbox::default(),
            partial: None,
            supervised: false,
        }
    }

    pub(crate) fn child(
        agent_id: String,
        parent_agent_id: String,
        depth: u32,
        run_id: String,
    ) -> Self {
        Self {
            agent_id,
            parent_agent_id: Some(parent_agent_id.clone()),
            depth,
            lifecycle: AgentLifecycle::Queued,
            current_run: Some(AgentRunState::new(
                run_id,
                AgentRunStatus::Queued,
                Some(parent_agent_id),
                depth,
            )),
            run_history: Vec::new(),
            mailbox: AgentMailbox::default(),
            partial: None,
            supervised: false,
        }
    }

    pub(crate) fn restored(
        agent_id: String,
        parent_agent_id: Option<String>,
        depth: u32,
        lifecycle: AgentLifecycle,
        current_run: Option<AgentRunState>,
    ) -> Self {
        Self {
            agent_id,
            parent_agent_id,
            depth,
            lifecycle,
            current_run,
            run_history: Vec::new(),
            mailbox: AgentMailbox::default(),
            partial: None,
            supervised: false,
        }
    }

    fn remember_run(&mut self, run: AgentRunState) {
        if let Some(previous) = self
            .run_history
            .iter_mut()
            .find(|previous| previous.run_id == run.run_id)
        {
            *previous = run;
        } else {
            self.run_history.push(run);
        }
    }

    fn remember_current_run(&mut self) {
        if let Some(current) = self.current_run.take() {
            self.remember_run(current);
        }
    }

    pub(crate) fn run(&self, run_id: &str) -> Option<&AgentRunState> {
        self.current_run
            .as_ref()
            .filter(|run| run.run_id == run_id)
            .or_else(|| {
                self.run_history
                    .iter()
                    .rev()
                    .find(|run| run.run_id == run_id)
            })
    }
}

/// Exclusive ownership of an agent's durable context and its pre-run budget
/// reservation. Releasing the guard does not change the node's lifecycle.
pub(crate) struct AgentWriterGuard {
    registry: AgentExecutionRegistry,
    agent_id: String,
    released: bool,
}

impl std::fmt::Debug for AgentWriterGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentWriterGuard")
            .field("agent_id", &self.agent_id)
            .field("released", &self.released)
            .finish()
    }
}

impl AgentWriterGuard {
    pub(crate) fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.registry
            .writers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.agent_id);
    }
}

impl Drop for AgentWriterGuard {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Clone)]
pub(crate) struct AgentExecutionRegistry {
    /// Store-free unit-test fallback ONLY. Whenever `runtime` is attached (all
    /// production sessions), `AgentRuntime` is the sole authority for agents,
    /// runs, lineage, closed state, and mailbox; `fallback_nodes` is untouched.
    fallback_nodes: Arc<Mutex<HashMap<String, AgentNode>>>,
    /// Ephemeral process-local live partial text per agent (never persisted).
    partials: Arc<Mutex<HashMap<String, AgentPartial>>>,
    /// Process-local active-driver supervision set backing `wait_for_idle`.
    supervised: Arc<Mutex<HashSet<String>>>,
    writers: Arc<Mutex<HashSet<String>>>,
    executions: Arc<tokio::sync::watch::Sender<usize>>,
    session_id: String,
    store: Option<Arc<Mutex<crate::store::Store>>>,
    runtime: Option<super::runtime::AgentRuntime>,
    backends: Arc<super::AgentBackendRegistry>,
}

fn runtime_mailbox_message(
    session_id: &str,
    agent_id: &str,
    run_id: &str,
    steer: &AgentSteer,
) -> Result<super::runtime::RuntimeMailboxMessage, String> {
    let notification_id = steer
        .notification
        .as_ref()
        .map(|notification| notification.id.clone());
    let payload = serde_json::to_value(super::runtime::RuntimeMailboxPayload {
        text: steer.text.clone(),
        notification: steer.notification.clone(),
    })
    .map_err(|error| error.to_string())?;
    Ok(super::runtime::RuntimeMailboxMessage::new(
        session_id.to_string(),
        steer.id.clone(),
        agent_id.to_string(),
        Some(run_id.to_string()),
        if steer.notification.is_some() {
            super::runtime::MailboxMessageKind::Notification
        } else {
            super::runtime::MailboxMessageKind::Steer
        },
        payload,
        notification_id,
    ))
}

fn steer_from_runtime_mailbox(
    message: super::runtime::RuntimeMailboxMessage,
) -> Result<AgentSteer, String> {
    let payload: super::runtime::RuntimeMailboxPayload =
        serde_json::from_value(message.payload).map_err(|error| error.to_string())?;
    Ok(AgentSteer::restored(
        message.message_id,
        payload.text,
        payload.notification,
    ))
}

impl Default for AgentExecutionRegistry {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl AgentExecutionRegistry {
    pub(crate) fn new(session_id: String) -> Self {
        Self::with_backends(
            session_id,
            Arc::new(super::AgentBackendRegistry::with_default()),
        )
    }

    pub(crate) fn with_backends(
        session_id: String,
        backends: Arc<super::AgentBackendRegistry>,
    ) -> Self {
        Self {
            fallback_nodes: Arc::new(Mutex::new(HashMap::new())),
            partials: Arc::new(Mutex::new(HashMap::new())),
            supervised: Arc::new(Mutex::new(HashSet::new())),
            writers: Arc::new(Mutex::new(HashSet::new())),
            executions: Arc::new(tokio::sync::watch::channel(0).0),
            session_id,
            store: None,
            runtime: None,
            backends,
        }
    }

    pub(crate) fn attach_store(&mut self, store: Arc<Mutex<crate::store::Store>>) {
        self.runtime = Some(
            super::runtime::AgentRuntime::spawn_root_with_backends(
                self.session_id.clone(),
                store.clone(),
                self.backends.clone(),
            )
            .expect("agent runtime control store"),
        );
        self.store = Some(store);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, AgentNode>> {
        self.fallback_nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn supervise_id(&self, agent_id: &str) {
        let mut set = self
            .supervised
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if set.insert(agent_id.to_string()) {
            self.executions.send_modify(|count| *count += 1);
        }
    }

    fn finish_supervision_id(&self, agent_id: &str) {
        let mut set = self
            .supervised
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if set.remove(agent_id) {
            self.executions
                .send_modify(|count| *count = count.saturating_sub(1));
        }
    }

    fn runtime_run_status(status: super::runtime::RuntimeRunStatus) -> AgentRunStatus {
        match status {
            super::runtime::RuntimeRunStatus::Queued => AgentRunStatus::Queued,
            super::runtime::RuntimeRunStatus::Running
            | super::runtime::RuntimeRunStatus::Stopping => AgentRunStatus::Running,
            super::runtime::RuntimeRunStatus::Completed => AgentRunStatus::Completed,
            super::runtime::RuntimeRunStatus::Cancelled => AgentRunStatus::Cancelled,
            super::runtime::RuntimeRunStatus::Failed => AgentRunStatus::Failed,
            super::runtime::RuntimeRunStatus::Lost => AgentRunStatus::Lost,
        }
    }

    fn project_runtime_nodes(&self, snapshot: super::runtime::RuntimeSnapshotView) -> Vec<AgentNode> {
        let partials = self
            .partials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let supervised = self
            .supervised
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut runs_by_agent: HashMap<String, Vec<super::runtime::RuntimeRun>> = HashMap::new();
        for run in snapshot.runs {
            runs_by_agent
                .entry(run.agent_id.clone())
                .or_default()
                .push(run);
        }
        let mut mail_by_agent: HashMap<String, AgentMailbox> = HashMap::new();
        for message in snapshot.mailbox {
            if message.state == super::runtime::MailboxDeliveryState::Consumed {
                continue;
            }
            if let Ok(steer) = steer_from_runtime_mailbox(message.clone()) {
                mail_by_agent
                    .entry(message.target_agent_id)
                    .or_default()
                    .restore(steer);
            }
        }
        snapshot
            .agents
            .into_iter()
            .map(|agent| {
                let mut runs = runs_by_agent.remove(&agent.agent_id).unwrap_or_default();
                runs.sort_by(|a, b| {
                    a.started_at_ms
                        .cmp(&b.started_at_ms)
                        .then_with(|| a.run_id.cmp(&b.run_id))
                });
                let latest = runs.pop();
                let current_run = latest.as_ref().map(|run| {
                    AgentRunState::new(
                        run.run_id.clone(),
                        Self::runtime_run_status(run.status),
                        agent.parent_agent_id.clone(),
                        agent.depth,
                    )
                });
                let run_history = runs
                    .into_iter()
                    .map(|run| {
                        AgentRunState::new(
                            run.run_id,
                            Self::runtime_run_status(run.status),
                            agent.parent_agent_id.clone(),
                            agent.depth,
                        )
                    })
                    .collect();
                let lifecycle = if agent.closed_at_ms.is_some() {
                    AgentLifecycle::Closed
                } else {
                    match latest.as_ref().map(|r| r.status) {
                        Some(super::runtime::RuntimeRunStatus::Queued) => AgentLifecycle::Queued,
                        Some(
                            super::runtime::RuntimeRunStatus::Running
                            | super::runtime::RuntimeRunStatus::Stopping,
                        ) => AgentLifecycle::Running,
                        Some(super::runtime::RuntimeRunStatus::Lost) => AgentLifecycle::Lost,
                        _ => AgentLifecycle::Idle,
                    }
                };
                AgentNode {
                    partial: partials.get(&agent.agent_id).cloned(),
                    supervised: supervised.contains(&agent.agent_id),
                    mailbox: mail_by_agent.remove(&agent.agent_id).unwrap_or_default(),
                    agent_id: agent.agent_id,
                    parent_agent_id: agent.parent_agent_id,
                    depth: agent.depth,
                    lifecycle,
                    current_run,
                    run_history,
                }
            })
            .collect()
    }

    pub(crate) fn claim_writer(&self, agent_id: &str) -> Option<AgentWriterGuard> {
        let mut writers = self
            .writers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !writers.insert(agent_id.to_string()) {
            return None;
        }
        Some(AgentWriterGuard {
            registry: self.clone(),
            agent_id: agent_id.to_string(),
            released: false,
        })
    }

    pub(crate) fn writer_active(&self, agent_id: &str) -> bool {
        self.writers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(agent_id)
    }

    pub(crate) fn child_count(&self) -> usize {
        let mut ids: HashSet<String> = if let Some(snapshot) = self.runtime_snapshot() {
            snapshot
                .agents
                .into_iter()
                .filter(|agent| agent.depth > 0 && agent.closed_at_ms.is_none())
                .map(|agent| agent.agent_id)
                .collect()
        } else {
            self.lock()
                .values()
                .filter(|node| node.depth > 0 && node.lifecycle != AgentLifecycle::Closed)
                .map(|node| node.agent_id.clone())
                .collect()
        };
        ids.extend(
            self.writers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .cloned(),
        );
        ids.len()
    }

    fn supervise(&self, node: &mut AgentNode) {
        if node.supervised {
            return;
        }
        node.supervised = true;
        self.executions.send_modify(|count| *count += 1);
    }

    fn finish_supervision(&self, node: &mut AgentNode) {
        if !node.supervised {
            return;
        }
        node.supervised = false;
        self.executions
            .send_modify(|count| *count = count.saturating_sub(1));
    }

    pub(crate) async fn wait_for_idle(&self) {
        let mut executions = self.executions.subscribe();
        let _ = executions
            .wait_for(|count| *count == 0)
            .await
            .expect("the registry retains the execution sender");
    }

    pub(crate) fn register_root(&self, agent_id: String) {
        if let Some(runtime) = &self.runtime {
            if runtime
                .snapshot_blocking()
                .ok()
                .is_some_and(|snapshot| snapshot.agents.iter().any(|a| a.agent_id == agent_id))
            {
                return;
            }
            let _ = runtime.register_agent_blocking(super::runtime::RuntimeAgent::root(
                agent_id,
                "main".to_string(),
                crate::model_io::now_ms(),
            ));
            return;
        }
        self.lock()
            .entry(agent_id.clone())
            .or_insert_with(|| AgentNode::root(agent_id));
    }

    pub(crate) fn attach_event_journal(
        &self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
    ) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let mut events = runtime.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if event_tx
                            .send(pig_protocol::Event::AgentControl {
                                event: event.protocol(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "agent control event consumer lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    pub(crate) fn runtime_snapshot(&self) -> Option<super::runtime::RuntimeSnapshotView> {
        let runtime = self.runtime.as_ref()?;
        match runtime.snapshot_blocking() {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                tracing::warn!(%error, "runtime snapshot failed");
                None
            }
        }
    }

    pub(crate) fn execution_runtime(&self) -> Option<super::runtime::AgentRuntime> {
        self.runtime.clone()
    }

    pub(crate) fn update_agent_context(
        &self,
        agent_id: &str,
        profile: String,
        conversation_path: std::path::PathBuf,
    ) -> bool {
        let Some(runtime) = &self.runtime else {
            return true;
        };
        let result = (|| {
            let snapshot = runtime.snapshot_blocking()?;
            let mut agent = snapshot
                .agents
                .into_iter()
                .find(|agent| agent.agent_id == agent_id)
                .ok_or_else(|| {
                    super::runtime::RuntimeError::NotFound(format!(
                        "Agent {agent_id} is not registered"
                    ))
                })?;
            agent.profile = profile;
            agent.conversation_path = Some(conversation_path);
            runtime.register_agent_blocking(agent)
        })();
        if let Err(error) = result {
            tracing::warn!(%agent_id, %error, "runtime agent context update failed");
            return false;
        }
        true
    }

    /// A child may be adopted by a new direct parent only between runs.
    /// An active run or a closed conversation cannot be taken over.
    pub(crate) fn activate_child(&self, mut child: AgentNode) -> Result<(), String> {
        if let Some(runtime) = &self.runtime {
            let snapshot = runtime
                .snapshot_blocking()
                .map_err(|error| error.to_string())?;
            if let Some(existing) = snapshot
                .agents
                .iter()
                .find(|agent| agent.agent_id == child.agent_id)
            {
                if existing.closed_at_ms.is_some() {
                    return Err(format!("Agent {} is closed", child.agent_id));
                }
                if snapshot
                    .runs
                    .iter()
                    .any(|run| run.agent_id == child.agent_id && run.status.is_active())
                {
                    return Err(format!("Agent {} is already running", child.agent_id));
                }
            }
            let current_run = child
                .current_run
                .clone()
                .ok_or_else(|| format!("Agent {} has no activation run", child.agent_id))?;
            let parent_agent_id = current_run
                .parent_agent_id
                .clone()
                .ok_or_else(|| format!("Child agent {} has no direct parent", child.agent_id))?;
            let parent_run_id = snapshot
                .runs
                .iter()
                .filter(|run| run.agent_id == parent_agent_id && run.status.is_active())
                .max_by_key(|run| (run.started_at_ms, run.run_id.as_str()))
                .map(|run| run.run_id.clone())
                .unwrap_or_default();
            runtime
                .register_agent_blocking(super::runtime::RuntimeAgent {
                    session_id: self.session_id.clone(),
                    agent_id: child.agent_id.clone(),
                    parent_agent_id: Some(parent_agent_id.clone()),
                    depth: child.depth,
                    profile: String::new(),
                    conversation_path: None,
                    created_at_ms: crate::model_io::now_ms(),
                    closed_at_ms: None,
                })
                .map_err(|error| error.to_string())?;
            runtime
                .start_run_blocking(super::runtime::StartRun {
                    run_id: Some(current_run.run_id.clone()),
                    agent_id: child.agent_id.clone(),
                    backend_id: "local".to_string(),
                    required_capabilities: Default::default(),
                    initiator: super::runtime::AgentInitiator::Agent {
                        agent_id: parent_agent_id,
                        run_id: parent_run_id,
                    },
                    idempotency_key: Some(format!("agent-run:{}", current_run.run_id)),
                    causation_id: Some(current_run.run_id.clone()),
                    correlation_id: Some(self.session_id.clone()),
                    queued: true,
                    watchdog: Default::default(),
                })
                .map_err(|error| error.to_string())?;
            self.supervise_id(&child.agent_id);
            return Ok(());
        }
        let mut nodes = self.lock();
        if let Some(existing) = nodes.get(&child.agent_id) {
            if existing.lifecycle.is_active() {
                return Err(format!("Agent {} is already running", child.agent_id));
            }
            if existing.lifecycle == AgentLifecycle::Closed {
                return Err(format!("Agent {} is closed", child.agent_id));
            }
        }
        if let Some(mut existing) = nodes.remove(&child.agent_id) {
            existing.remember_current_run();
            child.mailbox = existing.mailbox;
            child.run_history = existing.run_history;
        }
        self.supervise(&mut child);
        nodes.insert(child.agent_id.clone(), child);
        Ok(())
    }

    pub(crate) fn begin_run(&self, agent_id: &str, run_id: String) -> bool {
        if let Some(runtime) = &self.runtime {
            let started = runtime.start_run_blocking(super::runtime::StartRun {
                run_id: Some(run_id.clone()),
                agent_id: agent_id.to_string(),
                backend_id: "local".to_string(),
                required_capabilities: Default::default(),
                initiator: super::runtime::AgentInitiator::User { request_id: None },
                idempotency_key: Some(format!("agent-run:{run_id}")),
                causation_id: Some(run_id.clone()),
                correlation_id: Some(self.session_id.clone()),
                queued: false,
                watchdog: Default::default(),
            });
            match started {
                Ok((run, _)) if run.status == super::runtime::RuntimeRunStatus::Queued => {
                    if let Err(error) = runtime.mark_run_running_blocking(run_id.clone()) {
                        tracing::warn!(%agent_id, %run_id, %error, "runtime run start was rejected");
                        return false;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%agent_id, %run_id, %error, "runtime run admission failed");
                    return false;
                }
            }
            self.supervise_id(agent_id);
            return true;
        }
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        if node.lifecycle == AgentLifecycle::Closed
            || (node.lifecycle.is_active()
                && node
                    .current_run
                    .as_ref()
                    .is_some_and(|run| run.run_id != run_id))
        {
            return false;
        }
        if node
            .current_run
            .as_ref()
            .is_some_and(|run| run.run_id != run_id)
        {
            node.remember_current_run();
        }
        let parent_agent_id = node.parent_agent_id.clone();
        let depth = node.depth;
        self.supervise(node);
        node.lifecycle = AgentLifecycle::Running;
        node.current_run = Some(AgentRunState::new(
            run_id,
            AgentRunStatus::Running,
            parent_agent_id,
            depth,
        ));
        true
    }

    pub(crate) fn register_run_cancel(
        &self,
        run_id: String,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        let Some(runtime) = &self.runtime else {
            return Ok(());
        };
        runtime
            .register_execution_cancel_blocking(run_id, cancel)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn touch_activity(&self, agent_id: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let Some(run_id) = self.active_runtime_run_id(agent_id) else {
            return;
        };
        let runtime = runtime.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = runtime.touch_run_activity(run_id).await;
            });
        } else if let Err(error) = runtime.touch_run_activity_blocking(run_id) {
            tracing::warn!(%error, "runtime activity update failed");
        }
    }

    pub(crate) fn settle_run(&self, agent_id: &str, run_id: &str, status: AgentRunStatus) -> bool {
        self.settle_run_with_result(agent_id, run_id, status, None, (0, 0, 0))
    }

    pub(crate) fn settle_run_with_result(
        &self,
        agent_id: &str,
        run_id: &str,
        status: AgentRunStatus,
        stop_reason: Option<String>,
        usage: (u64, u64, u64),
    ) -> bool {
        self.settle_run_with_outcome(agent_id, run_id, status, stop_reason, usage, 0)
    }

    pub(crate) fn settle_run_with_outcome(
        &self,
        agent_id: &str,
        run_id: &str,
        status: AgentRunStatus,
        stop_reason: Option<String>,
        usage: (u64, u64, u64),
        turns: usize,
    ) -> bool {
        if let Some(runtime) = &self.runtime {
            let runtime_status = match status {
                AgentRunStatus::Completed => super::runtime::RuntimeRunStatus::Completed,
                AgentRunStatus::Cancelled => super::runtime::RuntimeRunStatus::Cancelled,
                AgentRunStatus::Failed => super::runtime::RuntimeRunStatus::Failed,
                AgentRunStatus::Lost => super::runtime::RuntimeRunStatus::Lost,
                AgentRunStatus::Queued | AgentRunStatus::Running => return false,
            };
            if let Err(error) = runtime.settle_current_run_with_turns_blocking(
                run_id.to_string(),
                runtime_status,
                Some(stop_reason.unwrap_or_else(|| status.as_str().to_string())),
                usage,
                turns,
            ) {
                tracing::warn!(%agent_id, %run_id, %error, "runtime run settlement failed");
                return false;
            }
            self.finish_supervision_id(agent_id);
            return true;
        }
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        if node
            .current_run
            .as_ref()
            .is_none_or(|run| run.run_id != run_id)
        {
            return false;
        }
        if let Some(run) = node.current_run.as_mut() {
            run.status = status;
        }
        node.lifecycle = if node.lifecycle == AgentLifecycle::Closed {
            AgentLifecycle::Closed
        } else if status == AgentRunStatus::Lost {
            AgentLifecycle::Lost
        } else {
            AgentLifecycle::Idle
        };
        self.finish_supervision(node);
        true
    }

    pub(crate) fn restore(&self, mut node: AgentNode) {
        if self.runtime.is_some() {
            // Authoritative agent/run state lives in AgentRuntime (restored from SQLite).
            return;
        }
        let mut nodes = self.lock();
        if let Some(mut previous) = nodes.remove(&node.agent_id) {
            if previous.supervised {
                nodes.insert(previous.agent_id.clone(), previous);
                return;
            }
            previous.remember_current_run();
            for run in previous.run_history {
                if let Some(existing) = node
                    .run_history
                    .iter_mut()
                    .find(|existing| existing.run_id == run.run_id)
                {
                    *existing = run;
                } else {
                    node.run_history.push(run);
                }
            }
            node.mailbox = previous.mailbox;
        }
        nodes.insert(node.agent_id.clone(), node);
    }

    pub(crate) fn request_run_stop(&self, run_id: &str, reason: &str) -> Result<(), String> {
        let Some(runtime) = &self.runtime else {
            return Ok(());
        };
        runtime
            .request_stop_current_blocking(run_id.to_string(), reason.to_string())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn close(&self, agent_id: &str) -> bool {
        if let Some(runtime) = &self.runtime {
            if let Some(run_id) = self.active_runtime_run_id(agent_id)
                && let Err(error) = self.request_run_stop(&run_id, "Session is closing")
            {
                tracing::warn!(%agent_id, %run_id, %error, "runtime stop request failed");
                return false;
            }
            return match runtime.close_agent_blocking(agent_id.to_string()) {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(%agent_id, %error, "runtime close agent failed");
                    false
                }
            };
        }
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        node.lifecycle = AgentLifecycle::Closed;
        true
    }

    pub(crate) fn close_all(&self) {
        if let Some(snapshot) = self.runtime_snapshot() {
            for agent in snapshot.agents {
                self.close(&agent.agent_id);
            }
            return;
        }
        let ids: Vec<String> = self.lock().keys().cloned().collect();
        for agent_id in ids {
            self.close(&agent_id);
        }
    }

    #[cfg(test)]
    fn shutdown_runtime(&self) {
        if let Some(runtime) = &self.runtime {
            runtime.shutdown_blocking().expect("runtime shutdown");
        }
    }

    #[cfg(test)]
    pub(crate) fn node(&self, agent_id: &str) -> Option<AgentNode> {
        if let Some(snapshot) = self.runtime_snapshot() {
            return self
                .project_runtime_nodes(snapshot)
                .into_iter()
                .find(|node| node.agent_id == agent_id);
        }
        self.lock().get(agent_id).cloned()
    }

    pub(crate) fn run_status(&self, agent_id: &str, run_id: &str) -> Option<AgentRunStatus> {
        if let Some(snapshot) = self.runtime_snapshot() {
            return snapshot
                .runs
                .iter()
                .find(|run| run.agent_id == agent_id && run.run_id == run_id)
                .map(|run| Self::runtime_run_status(run.status));
        }
        self.lock()
            .get(agent_id)
            .and_then(|node| node.current_run.as_ref())
            .filter(|run| run.run_id == run_id)
            .map(|run| run.status)
    }

    pub(crate) fn is_active(&self, agent_id: &str) -> bool {
        if let Some(snapshot) = self.runtime_snapshot() {
            let closed = snapshot
                .agents
                .iter()
                .find(|agent| agent.agent_id == agent_id)
                .is_some_and(|agent| agent.closed_at_ms.is_some());
            if closed {
                return false;
            }
            return snapshot
                .runs
                .iter()
                .any(|run| run.agent_id == agent_id && run.status.is_active());
        }
        self.lock()
            .get(agent_id)
            .is_some_and(|node| node.lifecycle.is_active())
    }

    fn active_runtime_run_id(&self, agent_id: &str) -> Option<String> {
        self.runtime_snapshot()?
            .runs
            .into_iter()
            .filter(|run| run.agent_id == agent_id && run.status.is_active())
            .max_by_key(|run| (run.started_at_ms, run.run_id.clone()))
            .map(|run| run.run_id)
    }

    pub(crate) fn push_terminal_notification(
        &self,
        agent_id: &str,
        task: &crate::store::BackgroundTask,
        steer: AgentSteer,
    ) -> Result<(), String> {
        if let Some(runtime) = &self.runtime {
            let run_id = self
                .active_runtime_run_id(agent_id)
                .ok_or_else(|| format!("Agent {agent_id} is not active"))?;
            let message = runtime_mailbox_message(&self.session_id, agent_id, &run_id, &steer)?;
            runtime
                .enqueue_task_notification_blocking(task.clone(), message)
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        let mut nodes = self.lock();
        let node = nodes
            .get_mut(agent_id)
            .ok_or_else(|| format!("Agent {agent_id} is not registered"))?;
        if !node.lifecycle.is_active() {
            return Err(format!("Agent {agent_id} is not active"));
        }
        if let Some(store) = &self.store {
            store
                .lock()
                .expect("store lock")
                .upsert_background_task(&self.session_id, task)
                .map_err(|error| pig_provider::core_error_en(&error))?;
        }
        node.mailbox.push(steer);
        Ok(())
    }

    pub(crate) fn record_root_terminal(
        &self,
        task: &crate::store::BackgroundTask,
        notification: &pig_protocol::TaskNotification,
    ) -> Result<(), pig_protocol::CoreError> {
        if let Some(runtime) = &self.runtime {
            return runtime
                .consume_notification_with_task_blocking(task.clone(), notification.clone())
                .map(|_| ())
                .map_err(|error| pig_protocol::CoreError::TaskStore {
                    detail: error.to_string(),
                });
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| pig_protocol::CoreError::TaskStore {
                detail: "Agent registry has no task store".to_string(),
            })?;
        store
            .lock()
            .expect("store lock")
            .record_terminal_task(&self.session_id, task, notification)
    }

    pub(crate) fn push_steer(&self, agent_id: &str, steer: AgentSteer) -> Result<(), String> {
        if let Some(runtime) = &self.runtime {
            let run_id = self
                .active_runtime_run_id(agent_id)
                .ok_or_else(|| format!("Agent {agent_id} is not active"))?;
            let message = runtime_mailbox_message(&self.session_id, agent_id, &run_id, &steer)?;
            runtime
                .enqueue_mailbox_blocking(message)
                .map_err(|error| format!("Failed to persist Agent mailbox message: {error}"))?;
            return Ok(());
        }
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return Err(format!("Agent {agent_id} is not registered"));
        };
        if !node.lifecycle.is_active() {
            return Err(format!("Agent {agent_id} is not active"));
        }
        let Some(_run) = &node.current_run else {
            return Err(format!("Agent {agent_id} has no current run"));
        };
        node.mailbox.push(steer);
        Ok(())
    }

    pub(crate) fn prepend_steer(&self, agent_id: &str, steer: AgentSteer) -> bool {
        if let Some(runtime) = &self.runtime {
            return match runtime.requeue_mailbox_blocking(steer.id) {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(%agent_id, %error, "mailbox requeue failed");
                    false
                }
            };
        }
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        if !node.lifecycle.is_active() {
            return false;
        }
        node.mailbox.prepend(steer);
        true
    }

    pub(crate) fn drain_mailbox(&self, agent_id: &str) -> Vec<AgentSteer> {
        let Some(runtime) = &self.runtime else {
            return self
                .lock()
                .get_mut(agent_id)
                .map(|node| node.mailbox.drain())
                .unwrap_or_default();
        };
        let claimed = match runtime.claim_mailbox_blocking(agent_id.to_string()) {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::warn!(%agent_id, %error, "mailbox claim failed");
                return Vec::new();
            }
        };
        let mut drained = Vec::with_capacity(claimed.len());
        for message in claimed {
            match steer_from_runtime_mailbox(message) {
                Ok(steer) => drained.push(steer),
                Err(error) => {
                    tracing::warn!(%agent_id, %error, "mailbox payload is invalid");
                    return Vec::new();
                }
            }
        }
        drained
    }

    pub(crate) fn consume_steer(&self, message_id: &str) -> Result<(), String> {
        if let Some(runtime) = &self.runtime {
            return runtime
                .consume_mailbox_blocking(message_id.to_string())
                .map(|_| ())
                .map_err(|error| format!("Failed to acknowledge Agent mailbox message: {error}"));
        }
        Ok(())
    }

    pub(crate) fn consume_notification(&self, notification_id: &str) -> Result<(), String> {
        let Some(runtime) = &self.runtime else {
            return Ok(());
        };
        runtime
            .consume_notification_blocking(notification_id.to_string())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn drain_notifications(&self, agent_id: &str) -> Vec<AgentSteer> {
        if let Some(runtime) = &self.runtime {
            let claimed = match runtime.claim_mailbox_blocking(agent_id.to_string()) {
                Ok(claimed) => claimed,
                Err(error) => {
                    tracing::warn!(%agent_id, %error, "notification claim failed");
                    return Vec::new();
                }
            };
            let mut notifications = Vec::new();
            for message in claimed {
                if message.kind == super::runtime::MailboxMessageKind::Notification {
                    if let Ok(steer) = steer_from_runtime_mailbox(message) {
                        notifications.push(steer);
                    }
                } else {
                    let _ = runtime.requeue_mailbox_blocking(message.message_id);
                }
            }
            return notifications;
        }
        self.lock()
            .get_mut(agent_id)
            .map(|node| node.mailbox.drain_notifications())
            .unwrap_or_default()
    }

    pub(crate) fn restore_mailbox(&self) -> Result<(), pig_protocol::CoreError> {
        if let Some(runtime) = &self.runtime {
            let snapshot = runtime.snapshot_blocking().map_err(|error| {
                pig_protocol::CoreError::TaskStore {
                    detail: error.to_string(),
                }
            })?;
            for agent in &snapshot.agents {
                let delivered = match &agent.conversation_path {
                    Some(path) if path.is_file() => crate::agent::agent_mailbox_deliveries(path)
                        .map_err(|detail| pig_protocol::CoreError::TaskStore { detail })?,
                    _ => Default::default(),
                };
                runtime
                    .reconcile_mailbox_blocking(agent.agent_id.clone(), delivered)
                    .map_err(|error| pig_protocol::CoreError::TaskStore {
                        detail: error.to_string(),
                    })?;
            }
            let pending = runtime
                .snapshot_blocking()
                .map_err(|error| pig_protocol::CoreError::TaskStore {
                    detail: error.to_string(),
                })?
                .mailbox
                .into_iter()
                .filter(|message| message.state != super::runtime::MailboxDeliveryState::Consumed)
                .collect::<Vec<_>>();
            for message in pending {
                if let Some(notification_id) = &message.notification_id
                    && let Some(store) = &self.store
                    && store
                        .lock()
                        .expect("store lock")
                        .task_notification_exists(notification_id)?
                {
                    runtime
                        .consume_notification_blocking(notification_id.clone())
                        .map_err(|error| pig_protocol::CoreError::TaskStore {
                            detail: error.to_string(),
                        })?;
                }
            }
            return Ok(());
        }
        Ok(())
    }

    pub(crate) fn retain_mailbox(&self, agent_id: &str, keep: impl FnMut(&AgentSteer) -> bool) {
        if self.runtime.is_some() {
            // Authoritative mailbox filtering happens via AgentRuntime consume/requeue.
            return;
        }
        if let Some(node) = self.lock().get_mut(agent_id) {
            node.mailbox.retain(keep);
        }
    }

    pub(crate) fn record_partial(&self, agent_id: &str, run_id: &str, text: String) -> bool {
        if self.runtime.is_some() {
            if self.active_runtime_run_id(agent_id).as_deref() != Some(run_id) {
                return false;
            }
            self.partials
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    agent_id.to_string(),
                    AgentPartial {
                        run_id: run_id.to_string(),
                        text,
                    },
                );
            return true;
        }
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        if node
            .current_run
            .as_ref()
            .is_none_or(|run| run.run_id != run_id)
        {
            return false;
        }
        node.partial = Some(AgentPartial {
            run_id: run_id.to_string(),
            text,
        });
        true
    }

    pub(crate) fn partial(&self, agent_id: &str) -> Option<AgentPartial> {
        if self.runtime.is_some() {
            return self
                .partials
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(agent_id)
                .cloned();
        }
        self.lock()
            .get(agent_id)
            .and_then(|node| node.partial.clone())
    }

    pub(crate) fn snapshot(&self) -> Vec<AgentNode> {
        if let Some(snapshot) = self.runtime_snapshot() {
            return self.project_runtime_nodes(snapshot);
        }
        self.lock().values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_run_transitions_keep_identity_and_ignore_stale_settlement() {
        let registry = AgentExecutionRegistry::default();
        registry.register_root("root".into());
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate child");
        assert!(registry.is_active("child"));
        assert!(registry.begin_run("child", "run-1".into()));
        assert!(!registry.settle_run("child", "run-old", AgentRunStatus::Failed));
        assert!(registry.settle_run("child", "run-1", AgentRunStatus::Completed));
        assert!(!registry.is_active("child"));

        let node = registry.node("child").expect("child node");
        assert_eq!(node.parent_agent_id.as_deref(), Some("root"));
        assert_eq!(node.depth, 1);
        assert_eq!(node.lifecycle, AgentLifecycle::Idle);
        assert_eq!(
            node.current_run,
            Some(AgentRunState::new(
                "run-1".into(),
                AgentRunStatus::Completed,
                Some("root".into()),
                1,
            ))
        );
    }

    #[test]
    fn mailbox_delivery_requires_an_active_node_and_drains_at_settlement() {
        let registry = AgentExecutionRegistry::default();
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate child");
        assert!(
            registry
                .push_steer("child", AgentSteer::message("work".into()))
                .is_ok()
        );
        assert!(
            registry
                .push_steer("missing", AgentSteer::message("work".into()))
                .is_err()
        );
        registry.settle_run("child", "run-1", AgentRunStatus::Completed);
        assert!(
            registry
                .push_steer("child", AgentSteer::message("late".into()))
                .is_err()
        );
        assert_eq!(registry.drain_mailbox("child").len(), 1);
        assert!(registry.drain_mailbox("child").is_empty());
    }

    #[test]
    fn pending_steer_restores_into_a_rebuilt_agent_node() {
        let dir = std::env::temp_dir().join(format!(
            "pig-agent-registry-mailbox-{}-{}",
            std::process::id(),
            crate::model_io::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Mutex::new(crate::store::Store::open(&dir).expect("store")));
        let mut registry = AgentExecutionRegistry::new("s1".into());
        registry.attach_store(store.clone());
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "s1".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate child");
        registry
            .push_steer("child", AgentSteer::message("pending".into()))
            .expect("durable enqueue");
        let steer_id = registry
            .drain_mailbox("child")
            .into_iter()
            .next()
            .expect("pending")
            .id;

        // Requeue without consuming to model a process ending with pending input.
        registry.prepend_steer(
            "child",
            AgentSteer::restored(steer_id.clone(), "pending".into(), None),
        );
        registry.shutdown_runtime();
        let mut restored = AgentExecutionRegistry::new("s1".into());
        restored.attach_store(store);
        restored.restore(AgentNode::restored(
            "child".into(),
            Some("s1".into()),
            1,
            AgentLifecycle::Lost,
            Some(AgentRunState::new(
                "run-1".into(),
                AgentRunStatus::Lost,
                Some("s1".into()),
                1,
            )),
        ));
        restored.restore_mailbox().expect("restore mailbox");
        restored
            .activate_child(AgentNode::child(
                "child".into(),
                "s1".into(),
                1,
                "run-2".into(),
            ))
            .expect("resume keeps pending mailbox");
        let pending = restored.drain_mailbox("child");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, steer_id);
        restored.consume_steer(&pending[0].id).expect("consume");
        let conversation_path = dir.join("child.jsonl");
        crate::agent::append_agent_mailbox_message(
            &conversation_path,
            steer_id,
            None,
            "pending".into(),
        )
        .expect("persist model-visible mailbox delivery");
        let runtime = restored.runtime.as_ref().expect("runtime");
        let mut restored_agent = runtime
            .snapshot_blocking()
            .expect("snapshot")
            .agents
            .into_iter()
            .find(|agent| agent.agent_id == "child")
            .expect("child runtime agent");
        restored_agent.conversation_path = Some(conversation_path);
        runtime
            .register_agent_blocking(restored_agent)
            .expect("conversation path");
        restored.restore_mailbox().expect("consume is durable");
        assert!(restored.drain_mailbox("child").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn jsonl_delivery_without_sqlite_ack_is_consumed_once_on_restart() {
        let dir = std::env::temp_dir().join(format!(
            "pig-jsonl-mailbox-restart-{}-{}",
            std::process::id(),
            crate::model_io::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Mutex::new(crate::store::Store::open(&dir).expect("store")));
        let mut registry = AgentExecutionRegistry::new("s1".into());
        registry.attach_store(store.clone());
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "s1".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate child");
        let conversation_path = dir.join("child.jsonl");
        let runtime = registry.runtime.as_ref().expect("runtime");
        let mut child = runtime
            .snapshot_blocking()
            .expect("snapshot")
            .agents
            .into_iter()
            .find(|agent| agent.agent_id == "child")
            .expect("child runtime agent");
        child.conversation_path = Some(conversation_path.clone());
        runtime.register_agent_blocking(child).expect("set path");
        registry
            .push_steer("child", AgentSteer::message("pending".into()))
            .expect("enqueue");
        let claimed = registry.drain_mailbox("child");
        assert_eq!(claimed.len(), 1);
        crate::agent::append_agent_mailbox_message(
            &conversation_path,
            claimed[0].id.clone(),
            None,
            "pending".into(),
        )
        .expect("JSONL commit before SQLite acknowledgment");
        registry.shutdown_runtime();

        let mut restored = AgentExecutionRegistry::new("s1".into());
        restored.attach_store(store);
        restored.restore(AgentNode::restored(
            "child".into(),
            Some("s1".into()),
            1,
            AgentLifecycle::Lost,
            Some(AgentRunState::new(
                "run-1".into(),
                AgentRunStatus::Lost,
                Some("s1".into()),
                1,
            )),
        ));
        restored.restore_mailbox().expect("reconcile JSONL commit");
        restored
            .restore_mailbox()
            .expect("idempotent reconciliation");
        assert!(restored.drain_mailbox("child").is_empty());
        assert_eq!(
            restored
                .runtime
                .as_ref()
                .expect("runtime")
                .snapshot_blocking()
                .expect("snapshot")
                .mailbox
                .iter()
                .filter(|message| {
                    message.state == super::super::runtime::MailboxDeliveryState::Consumed
                })
                .count(),
            1
        );
        restored.shutdown_runtime();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn task_wait_tool_records_are_mailbox_delivery_receipts() {
        let dir = std::env::temp_dir().join(format!(
            "pig-task-wait-receipt-{}-{}",
            std::process::id(),
            crate::model_io::now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create receipt directory");
        let path = dir.join("child.jsonl");
        crate::agent::append_agent_conversation_record(
            &path,
            &crate::rollout::RolloutRecord::ToolCall {
                execution_id: "execution-1".into(),
                tool: "TaskWait".into(),
                summary: "wait for task".into(),
                arguments: "{}".into(),
                output: "completed".into(),
                is_error: false,
                stopped: false,
                edit: None,
                agent_card: None,
                agent_cards: Vec::new(),
                images: Vec::new(),
                documents: Vec::new(),
                delivered_notification_ids: vec!["notification-task-wait".into()],
                recovery: None,
            },
        )
        .expect("persist TaskWait receipt");
        let delivered = crate::agent::agent_mailbox_deliveries(&path).expect("read receipt");
        assert!(delivered.message_ids.is_empty());
        assert!(
            delivered
                .notification_ids
                .contains("notification-task-wait")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn writer_reservation_is_exclusive_and_counts_once_toward_the_child_budget() {
        let registry = AgentExecutionRegistry::default();
        let mut writer = registry.claim_writer("child").expect("reserve writer");
        assert_eq!(registry.child_count(), 1);
        assert!(registry.claim_writer("child").is_none());
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate reserved child");
        assert_eq!(registry.child_count(), 1);
        writer.release();
        assert!(!registry.writer_active("child"));
        assert_eq!(registry.child_count(), 1);
        registry.close("child");
        assert_eq!(registry.child_count(), 0);
    }

    #[tokio::test]
    async fn close_waits_for_queued_and_running_descendants_to_settle() {
        let registry = AgentExecutionRegistry::default();
        for agent_id in ["queued", "running"] {
            registry
                .activate_child(AgentNode::child(
                    agent_id.into(),
                    "root".into(),
                    1,
                    format!("run-{agent_id}"),
                ))
                .expect("activate child");
        }
        registry.begin_run("running", "run-running".into());
        registry.close_all();
        let waiter = tokio::spawn({
            let registry = registry.clone();
            async move { registry.wait_for_idle().await }
        });
        assert!(registry.settle_run("queued", "run-queued", AgentRunStatus::Cancelled));
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        assert!(registry.settle_run("running", "run-running", AgentRunStatus::Completed));
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("idle notification")
            .expect("wait task");
        assert_eq!(
            registry.node("running").expect("closed child").lifecycle,
            AgentLifecycle::Closed
        );
    }

    #[test]
    fn partial_output_is_scoped_to_the_current_run() {
        let registry = AgentExecutionRegistry::default();
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate child");
        assert!(registry.record_partial("child", "run-1", "first".into()));
        assert!(!registry.record_partial("child", "run-old", "stale".into()));
        assert_eq!(
            registry.partial("child").expect("partial"),
            AgentPartial {
                run_id: "run-1".into(),
                text: "first".into(),
            }
        );
    }

    #[test]
    fn resume_replaces_the_current_run_but_not_the_agent_identity() {
        let registry = AgentExecutionRegistry::default();
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate child");
        registry.settle_run("child", "run-1", AgentRunStatus::Cancelled);
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "new-parent".into(),
                2,
                "run-2".into(),
            ))
            .expect("resume idle child");
        assert!(registry.is_active("child"));
        let node = registry.node("child").expect("child");
        assert_eq!(
            node.current_run,
            Some(AgentRunState::new(
                "run-2".into(),
                AgentRunStatus::Queued,
                Some("new-parent".into()),
                2,
            ))
        );
        assert_eq!(
            node.run("run-1")
                .and_then(|run| run.parent_agent_id.as_deref()),
            Some("root")
        );
        assert!(registry.close("child"));
        assert!(registry.settle_run("child", "run-2", AgentRunStatus::Cancelled));
        assert!(
            registry
                .activate_child(AgentNode::child(
                    "child".into(),
                    "root".into(),
                    1,
                    "run-3".into(),
                ))
                .is_err()
        );
        assert_eq!(
            registry.node("child").expect("child").lifecycle,
            AgentLifecycle::Closed
        );
    }
}
