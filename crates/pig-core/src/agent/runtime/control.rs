use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, mpsc, oneshot};

use super::model::*;
use crate::store::Store;

#[derive(Clone, Debug)]
pub struct StartRun {
    pub run_id: Option<String>,
    pub agent_id: String,
    pub backend_id: String,
    pub required_capabilities: std::collections::BTreeSet<super::super::AgentExecutionCapability>,
    pub initiator: AgentInitiator,
    pub idempotency_key: Option<String>,
    pub causation_id: Option<String>,
    pub correlation_id: Option<String>,
    pub queued: bool,
    pub watchdog: super::RunWatchdogPolicy,
}

#[derive(Clone, Debug)]
pub struct RuntimeSnapshotView {
    pub session_id: String,
    pub agents: Vec<RuntimeAgent>,
    pub runs: Vec<RuntimeRun>,
    pub attempts: Vec<ExecutionAttempt>,
    pub mailbox: Vec<RuntimeMailboxMessage>,
    pub latest_sequence: u64,
    pub closed: bool,
}

type Command = Box<dyn FnOnce(&mut RuntimeState) + Send + 'static>;

#[derive(Clone)]
pub struct AgentRuntime {
    session_id: String,
    commands: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<RuntimeControlEvent>,
    backends: Arc<super::super::AgentBackendRegistry>,
    pub(super) executions: super::execution::ExecutionTable,
}

pub(super) struct RuntimeState {
    session_id: String,
    store: Arc<Mutex<Store>>,
    agents: BTreeMap<String, RuntimeAgent>,
    runs: BTreeMap<String, RuntimeRun>,
    attempts: BTreeMap<String, ExecutionAttempt>,
    mailbox: BTreeMap<String, RuntimeMailboxMessage>,
    next_sequence: u64,
    closed: bool,
    events: broadcast::Sender<RuntimeControlEvent>,
    stop_hooks: BTreeMap<String, tokio_util::sync::CancellationToken>,
    watchdog_ticks: bool,
    projections: super::RuntimeProjectionRegistry,
}

impl AgentRuntime {
    pub fn spawn(session_id: String, store: Arc<Mutex<Store>>) -> Result<Self, RuntimeError> {
        Self::spawn_with_backends(
            session_id,
            store,
            Arc::new(super::super::AgentBackendRegistry::with_local()),
        )
    }

    pub fn spawn_with_backends(
        session_id: String,
        store: Arc<Mutex<Store>>,
        backends: Arc<super::super::AgentBackendRegistry>,
    ) -> Result<Self, RuntimeError> {
        backends.ensure_scheduler();
        let snapshot = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .runtime_snapshot(&session_id)?;
        let (commands, receiver) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(1024);
        let mut state = RuntimeState {
            agents: snapshot
                .agents
                .into_iter()
                .map(|agent| (agent.agent_id.clone(), agent))
                .collect(),
            runs: snapshot
                .runs
                .into_iter()
                .map(|run| (run.run_id.clone(), run))
                .collect(),
            attempts: snapshot
                .attempts
                .into_iter()
                .map(|attempt| (attempt.run_id.clone(), attempt))
                .collect(),
            mailbox: snapshot
                .mailbox
                .into_iter()
                .map(|message| (message.message_id.clone(), message))
                .collect(),
            next_sequence: snapshot.latest_sequence,
            session_id: session_id.clone(),
            store,
            closed: false,
            events: events.clone(),
            stop_hooks: BTreeMap::new(),
            watchdog_ticks: true,
            projections: Default::default(),
        };
        state.recover_interrupted_local_runs()?;
        let projection_seed = state.view();
        state.projections = super::RuntimeProjectionRegistry::new(&projection_seed);
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("agent runtime command runtime")
                .block_on(run_commands(state, receiver));
        });
        Ok(Self {
            session_id,
            commands,
            events,
            backends,
            executions: Default::default(),
        })
    }

    pub fn spawn_root(session_id: String, store: Arc<Mutex<Store>>) -> Result<Self, RuntimeError> {
        Self::spawn_root_with_backends(
            session_id,
            store,
            Arc::new(super::super::AgentBackendRegistry::with_local()),
        )
    }

    pub fn spawn_root_with_backends(
        session_id: String,
        store: Arc<Mutex<Store>>,
        backends: Arc<super::super::AgentBackendRegistry>,
    ) -> Result<Self, RuntimeError> {
        let runtime = Self::spawn_with_backends(session_id.clone(), store, backends)?;
        if !runtime
            .snapshot_blocking()?
            .agents
            .iter()
            .any(|agent| agent.agent_id == session_id)
        {
            runtime.register_agent_blocking(RuntimeAgent::root(
                session_id,
                "main".to_string(),
                crate::model_io::now_ms(),
            ))?;
        }
        Ok(runtime)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn backends(&self) -> Arc<super::super::AgentBackendRegistry> {
        self.backends.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeControlEvent> {
        self.events.subscribe()
    }

    pub(super) async fn dispatch<R>(
        &self,
        operation: impl FnOnce(&mut RuntimeState) -> Result<R, RuntimeError> + Send + 'static,
    ) -> Result<R, RuntimeError>
    where
        R: Send + 'static,
    {
        let (reply, response) = oneshot::channel();
        let command = Box::new(move |state: &mut RuntimeState| {
            let result = if state.closed {
                Err(RuntimeError::Closed)
            } else {
                operation(state)
            };
            let _ = reply.send(result);
        });
        self.commands
            .send(command)
            .map_err(|_| RuntimeError::ChannelClosed)?;
        response.await.map_err(|_| RuntimeError::ChannelClosed)?
    }

    fn dispatch_blocking<R>(
        &self,
        operation: impl FnOnce(&mut RuntimeState) -> Result<R, RuntimeError> + Send + 'static,
    ) -> Result<R, RuntimeError>
    where
        R: Send + 'static,
    {
        let (reply, response) = std::sync::mpsc::channel();
        let command = Box::new(move |state: &mut RuntimeState| {
            let result = if state.closed {
                Err(RuntimeError::Closed)
            } else {
                operation(state)
            };
            let _ = reply.send(result);
        });
        self.commands
            .send(command)
            .map_err(|_| RuntimeError::ChannelClosed)?;
        response.recv().map_err(|_| RuntimeError::ChannelClosed)?
    }

    pub fn register_agent_blocking(
        &self,
        agent: RuntimeAgent,
    ) -> Result<RuntimeAgent, RuntimeError> {
        self.dispatch_blocking(move |state| state.register_agent(agent))
    }

    pub fn start_run_blocking(
        &self,
        request: StartRun,
    ) -> Result<(RuntimeRun, ExecutionAttempt), RuntimeError> {
        self.backends
            .preflight(&request.backend_id, &request.required_capabilities)
            .map_err(|error| RuntimeError::Backend(error.to_string()))?;
        self.dispatch_blocking(move |state| state.start_run(request))
    }

    pub fn mark_run_running_blocking(&self, run_id: String) -> Result<RuntimeRun, RuntimeError> {
        self.dispatch_blocking(move |state| state.mark_run_running(&run_id))
    }

    pub fn settle_current_run_blocking(
        &self,
        run_id: String,
        status: RuntimeRunStatus,
        stop_reason: Option<String>,
        usage: (u64, u64, u64),
    ) -> Result<RuntimeRun, RuntimeError> {
        self.settle_current_run_with_turns_blocking(run_id, status, stop_reason, usage, 0)
    }

    pub fn settle_current_run_with_turns_blocking(
        &self,
        run_id: String,
        status: RuntimeRunStatus,
        stop_reason: Option<String>,
        usage: (u64, u64, u64),
        turns: usize,
    ) -> Result<RuntimeRun, RuntimeError> {
        self.dispatch_blocking(move |state| {
            let attempt = state.attempts.get(&run_id).cloned().ok_or_else(|| {
                RuntimeError::NotFound(format!("Run {run_id} has no execution attempt"))
            })?;
            state.settle_run(
                &run_id,
                attempt.attempt,
                &attempt.backend_epoch,
                status,
                stop_reason,
                usage,
                turns,
            )
        })
    }

    pub fn release_execution_blocking(
        &self,
        run_id: String,
        attempt: u64,
        backend_epoch: String,
    ) -> Result<ExecutionAttempt, RuntimeError> {
        self.dispatch_blocking(move |state| {
            state.release_execution(&run_id, attempt, &backend_epoch)
        })
    }

    pub(crate) fn release_current_execution_blocking(
        &self,
        run_id: String,
    ) -> Result<ExecutionAttempt, RuntimeError> {
        self.dispatch_blocking(move |state| {
            let attempt = state.attempts.get(&run_id).cloned().ok_or_else(|| {
                RuntimeError::NotFound(format!("Run {run_id} has no execution attempt"))
            })?;
            state.release_execution(&run_id, attempt.attempt, &attempt.backend_epoch)
        })
    }

    pub fn request_stop_current_blocking(
        &self,
        run_id: String,
        reason: String,
    ) -> Result<RuntimeRun, RuntimeError> {
        self.dispatch_blocking(move |state| {
            let attempt = state.attempts.get(&run_id).cloned().ok_or_else(|| {
                RuntimeError::NotFound(format!("Run {run_id} has no execution attempt"))
            })?;
            state.request_stop(&run_id, attempt.attempt, &attempt.backend_epoch, reason)
        })
    }

    pub fn disconnect_run_blocking(
        &self,
        run_id: String,
    ) -> Result<ExecutionAttempt, RuntimeError> {
        self.dispatch_blocking(move |state| {
            state.set_connection_state(&run_id, RuntimeConnectionState::Disconnected)
        })
    }

    pub fn reconnect_run_blocking(
        &self,
        run_id: String,
        cursor: u64,
    ) -> Result<ExecutionAttempt, RuntimeError> {
        self.dispatch_blocking(move |state| state.reconnect_run(&run_id, cursor))
    }

    pub fn snapshot_blocking(&self) -> Result<RuntimeSnapshotView, RuntimeError> {
        self.dispatch_blocking(|state| Ok(state.view()))
    }

    pub fn register_projection_blocking(
        &self,
        definition: Box<dyn super::RuntimeProjectionDefinition>,
    ) -> Result<(), RuntimeError> {
        self.dispatch_blocking(move |state| {
            let snapshot = state.view();
            state
                .projections
                .register(definition, &snapshot)
                .map_err(|error| RuntimeError::Conflict(error.to_string()))
        })
    }

    pub fn resync_projection_blocking(&self, key: String) -> Result<(), RuntimeError> {
        self.dispatch_blocking(move |state| {
            let snapshot = state.view();
            state
                .projections
                .resync(&key, &snapshot)
                .map_err(|error| RuntimeError::Conflict(error.to_string()))
        })
    }

    pub fn projection_snapshot_blocking(
        &self,
    ) -> Result<super::RuntimeProjectionSnapshot, RuntimeError> {
        self.dispatch_blocking(|state| Ok(state.projections.snapshot()))
    }

    pub fn shutdown_blocking(&self) -> Result<(), RuntimeError> {
        self.dispatch_blocking(|state| {
            state.closed = true;
            Ok(())
        })
    }

    pub fn enqueue_mailbox_blocking(
        &self,
        message: RuntimeMailboxMessage,
    ) -> Result<RuntimeMailboxMessage, RuntimeError> {
        self.dispatch_blocking(move |state| state.enqueue_mailbox(message))
    }

    pub fn claim_mailbox_blocking(
        &self,
        agent_id: String,
    ) -> Result<Vec<RuntimeMailboxMessage>, RuntimeError> {
        self.dispatch_blocking(move |state| state.claim_mailbox(&agent_id))
    }

    pub fn consume_mailbox_blocking(
        &self,
        message_id: String,
    ) -> Result<RuntimeMailboxMessage, RuntimeError> {
        self.dispatch_blocking(move |state| state.consume_mailbox(&message_id))
    }

    pub fn consume_notification_blocking(
        &self,
        notification_id: String,
    ) -> Result<Option<RuntimeMailboxMessage>, RuntimeError> {
        self.dispatch_blocking(move |state| {
            let Some(message_id) = state
                .mailbox
                .values()
                .find(|message| {
                    message.notification_id.as_deref() == Some(notification_id.as_str())
                })
                .map(|message| message.message_id.clone())
            else {
                return Ok(None);
            };
            state.consume_mailbox(&message_id).map(Some)
        })
    }

    pub fn enqueue_task_notification_blocking(
        &self,
        task: crate::store::BackgroundTask,
        message: RuntimeMailboxMessage,
    ) -> Result<RuntimeMailboxMessage, RuntimeError> {
        self.dispatch_blocking(move |state| state.enqueue_task_notification(task, message))
    }

    pub fn consume_notification_with_task_blocking(
        &self,
        task: crate::store::BackgroundTask,
        notification: pig_protocol::TaskNotification,
    ) -> Result<Option<RuntimeMailboxMessage>, RuntimeError> {
        self.dispatch_blocking(move |state| {
            state.consume_notification_with_task(task, notification)
        })
    }

    pub fn requeue_mailbox_blocking(
        &self,
        message_id: String,
    ) -> Result<RuntimeMailboxMessage, RuntimeError> {
        self.dispatch_blocking(move |state| state.requeue_mailbox(&message_id))
    }

    pub fn reconcile_mailbox_blocking(
        &self,
        agent_id: String,
        delivered: RuntimeMailboxDeliveryReceipt,
    ) -> Result<(), RuntimeError> {
        self.dispatch_blocking(move |state| state.reconcile_mailbox(&agent_id, &delivered))
    }

    pub fn register_execution_cancel_blocking(
        &self,
        run_id: String,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), RuntimeError> {
        self.dispatch_blocking(move |state| {
            let run = state
                .runs
                .get(&run_id)
                .ok_or_else(|| RuntimeError::NotFound(format!("Run {run_id} is not registered")))?;
            if run.status.is_terminal() {
                return Err(RuntimeError::InvalidTransition(format!(
                    "Run {run_id} is already {}",
                    run.status.as_str()
                )));
            }
            if run.status == RuntimeRunStatus::Stopping {
                cancel.cancel();
            }
            state.stop_hooks.insert(run_id, cancel);
            Ok(())
        })
    }

    pub fn record_run_activity_blocking(&self, run_id: String) -> Result<(), RuntimeError> {
        self.dispatch_blocking(move |state| state.record_run_activity(&run_id, false))
    }

    pub(crate) fn touch_run_activity_blocking(&self, run_id: String) -> Result<(), RuntimeError> {
        self.dispatch_blocking(move |state| state.record_run_activity(&run_id, true))
    }

    pub(crate) async fn touch_run_activity(&self, run_id: String) -> Result<(), RuntimeError> {
        self.dispatch(move |state| state.record_run_activity(&run_id, true))
            .await
    }

    #[cfg(test)]
    fn disable_watchdog_ticks_blocking(&self) {
        self.dispatch_blocking(|state| {
            state.watchdog_ticks = false;
            Ok(())
        })
        .expect("disable watchdog ticks");
    }

    #[cfg(test)]
    fn check_watchdogs_at_blocking(&self, now_ms: u64) {
        self.dispatch_blocking(move |state| {
            state.check_watchdogs(now_ms);
            Ok(())
        })
        .expect("check watchdogs at injected time");
    }

    pub fn check_watchdogs_blocking(&self) -> Result<(), RuntimeError> {
        self.dispatch_blocking(|state| {
            state.check_watchdogs(crate::model_io::now_ms());
            Ok(())
        })
    }

    pub async fn register_agent(&self, agent: RuntimeAgent) -> Result<RuntimeAgent, RuntimeError> {
        self.dispatch(move |state| state.register_agent(agent))
            .await
    }

    pub async fn start_run(
        &self,
        request: StartRun,
    ) -> Result<(RuntimeRun, ExecutionAttempt), RuntimeError> {
        self.backends
            .preflight(&request.backend_id, &request.required_capabilities)
            .map_err(|error| RuntimeError::Backend(error.to_string()))?;
        self.dispatch(move |state| state.start_run(request)).await
    }

    pub async fn request_stop(
        &self,
        run_id: String,
        attempt: u64,
        backend_epoch: String,
        reason: String,
    ) -> Result<RuntimeRun, RuntimeError> {
        self.dispatch(move |state| state.request_stop(&run_id, attempt, &backend_epoch, reason))
            .await
    }

    pub async fn settle_run(
        &self,
        run_id: String,
        attempt: u64,
        backend_epoch: String,
        status: RuntimeRunStatus,
        stop_reason: Option<String>,
        usage: (u64, u64, u64),
    ) -> Result<RuntimeRun, RuntimeError> {
        self.dispatch(move |state| {
            state.settle_run(
                &run_id,
                attempt,
                &backend_epoch,
                status,
                stop_reason,
                usage,
                0,
            )
        })
        .await
    }

    pub async fn snapshot(&self) -> Result<RuntimeSnapshotView, RuntimeError> {
        self.dispatch(|state| Ok(state.view())).await
    }

    pub async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.dispatch(|state| {
            state.closed = true;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
const WATCHDOG_TICK: std::time::Duration = std::time::Duration::from_millis(5);
#[cfg(not(test))]
const WATCHDOG_TICK: std::time::Duration = std::time::Duration::from_millis(250);

async fn run_commands(mut state: RuntimeState, mut commands: mpsc::UnboundedReceiver<Command>) {
    let mut watchdog = tokio::time::interval(WATCHDOG_TICK);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => {
                let Some(command) = command else { break; };
                command(&mut state);
                if state.closed {
                    break;
                }
            }
            _ = watchdog.tick() => {
                if state.watchdog_ticks {
                    state.check_watchdogs(crate::model_io::now_ms());
                }
            }
        }
    }
}

impl RuntimeState {
    fn publish(&mut self, event: RuntimeControlEvent) {
        if let Err(error) = self.projections.apply(&event) {
            let snapshot = self.view();
            match error {
                super::ProjectionError::Gap { .. } => {
                    tracing::warn!(%error, sequence = event.sequence, "runtime projection gap requires resync");
                    if let Err(resync_error) = self.projections.resync_all(&snapshot) {
                        tracing::error!(%resync_error, sequence = event.sequence, "runtime projection resync failed");
                    }
                }
                _ => {
                    tracing::error!(%error, sequence = event.sequence, "runtime projection update failed")
                }
            }
        }
        let _ = self.events.send(event);
    }

    fn commit_mailbox_message(
        &mut self,
        message: RuntimeMailboxMessage,
        kind: RuntimeEventKind,
    ) -> Result<RuntimeMailboxMessage, RuntimeError> {
        let previous_sequence = self.next_sequence;
        let event = self.event(
            kind,
            Some(message.target_agent_id.clone()),
            message.target_run_id.clone(),
            None,
            serde_json::json!({
                "message_id": message.message_id,
                "notification_id": message.notification_id,
                "state": message.state,
                "kind": message.kind,
            }),
        );
        let result = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commit_runtime_mailbox(&message, &event);
        if let Err(error) = result {
            self.next_sequence = previous_sequence;
            return Err(error);
        }
        self.mailbox
            .insert(message.message_id.clone(), message.clone());
        self.publish(event);
        Ok(message)
    }

    fn validate_mailbox_message(
        &self,
        message: &RuntimeMailboxMessage,
    ) -> Result<Option<RuntimeMailboxMessage>, RuntimeError> {
        if message.session_id != self.session_id {
            return Err(RuntimeError::Conflict(format!(
                "Mailbox message {} belongs to a different session",
                message.message_id
            )));
        }
        if !self.agents.contains_key(&message.target_agent_id) {
            return Err(RuntimeError::NotFound(format!(
                "Agent {} is not registered",
                message.target_agent_id
            )));
        }
        if let Some(existing) = self.mailbox.get(&message.message_id) {
            if existing.target_agent_id == message.target_agent_id
                && existing.kind == message.kind
                && existing.payload == message.payload
                && existing.notification_id == message.notification_id
            {
                return Ok(Some(existing.clone()));
            }
            return Err(RuntimeError::Conflict(format!(
                "Mailbox message {} already has different content",
                message.message_id
            )));
        }
        if let Some(notification_id) = &message.notification_id
            && self.mailbox.values().any(|existing| {
                existing.notification_id.as_deref() == Some(notification_id.as_str())
            })
        {
            return Err(RuntimeError::Conflict(format!(
                "Notification {notification_id} is already queued"
            )));
        }
        Ok(None)
    }

    fn enqueue_mailbox(
        &mut self,
        message: RuntimeMailboxMessage,
    ) -> Result<RuntimeMailboxMessage, RuntimeError> {
        if let Some(existing) = self.validate_mailbox_message(&message)? {
            return Ok(existing);
        }
        self.commit_mailbox_message(message, RuntimeEventKind::MailboxEnqueued)
    }

    fn enqueue_task_notification(
        &mut self,
        task: crate::store::BackgroundTask,
        message: RuntimeMailboxMessage,
    ) -> Result<RuntimeMailboxMessage, RuntimeError> {
        if let Some(existing) = self.validate_mailbox_message(&message)? {
            return Ok(existing);
        }
        let previous_sequence = self.next_sequence;
        let event = self.event(
            RuntimeEventKind::MailboxEnqueued,
            Some(message.target_agent_id.clone()),
            message.target_run_id.clone(),
            None,
            serde_json::json!({
                "message_id": message.message_id,
                "notification_id": message.notification_id,
                "task_id": task.summary.id,
            }),
        );
        let result = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commit_runtime_task_mailbox(&self.session_id, &task, &message, &event);
        if let Err(error) = result {
            self.next_sequence = previous_sequence;
            return Err(error);
        }
        self.mailbox
            .insert(message.message_id.clone(), message.clone());
        self.publish(event);
        Ok(message)
    }

    fn claim_mailbox(
        &mut self,
        agent_id: &str,
    ) -> Result<Vec<RuntimeMailboxMessage>, RuntimeError> {
        let mut pending: Vec<RuntimeMailboxMessage> = self
            .mailbox
            .values()
            .filter(|message| {
                message.target_agent_id == agent_id && message.state == MailboxDeliveryState::Queued
            })
            .cloned()
            .collect();
        pending.sort_by_key(|message| (message.created_at_ms, message.message_id.clone()));
        let mut claimed = Vec::with_capacity(pending.len());
        for mut message in pending {
            message.state = MailboxDeliveryState::Claimed;
            message.claimed_at_ms = Some(crate::model_io::now_ms());
            claimed.push(self.commit_mailbox_message(message, RuntimeEventKind::MailboxClaimed)?);
        }
        Ok(claimed)
    }

    fn consume_mailbox(&mut self, message_id: &str) -> Result<RuntimeMailboxMessage, RuntimeError> {
        let current = self.mailbox.get(message_id).ok_or_else(|| {
            RuntimeError::NotFound(format!("Mailbox message {message_id} is not registered"))
        })?;
        if current.state == MailboxDeliveryState::Consumed {
            return Ok(current.clone());
        }
        let mut message = current.clone();
        message.state = MailboxDeliveryState::Consumed;
        message.consumed_at_ms = Some(crate::model_io::now_ms());
        self.commit_mailbox_message(message, RuntimeEventKind::MailboxConsumed)
    }

    fn consume_notification_with_task(
        &mut self,
        task: crate::store::BackgroundTask,
        notification: pig_protocol::TaskNotification,
    ) -> Result<Option<RuntimeMailboxMessage>, RuntimeError> {
        let current = self
            .mailbox
            .values()
            .find(|message| message.notification_id.as_deref() == Some(notification.id.as_str()))
            .cloned();
        let mut message = current.clone();
        if let Some(value) = &mut message
            && value.state != MailboxDeliveryState::Consumed
        {
            value.state = MailboxDeliveryState::Consumed;
            value.consumed_at_ms = Some(crate::model_io::now_ms());
        }
        let changed = message
            .as_ref()
            .is_some_and(|value| Some(value) != current.as_ref());
        let previous_sequence = self.next_sequence;
        let event = if changed {
            let value = message.as_ref().expect("changed mailbox message");
            Some(self.event(
                RuntimeEventKind::MailboxConsumed,
                Some(value.target_agent_id.clone()),
                value.target_run_id.clone(),
                None,
                serde_json::json!({
                    "message_id": value.message_id,
                    "notification_id": value.notification_id,
                    "forwarded_to_root": true,
                }),
            ))
        } else {
            None
        };
        let result = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commit_runtime_root_notification(
                &self.session_id,
                &task,
                &notification,
                if changed { message.as_ref() } else { None },
                event.as_ref(),
            );
        if let Err(error) = result {
            self.next_sequence = previous_sequence;
            return Err(error);
        }
        if let Some(value) = &message
            && changed
        {
            self.mailbox.insert(value.message_id.clone(), value.clone());
        }
        if let Some(event) = event {
            self.publish(event);
        }
        Ok(message)
    }

    fn requeue_mailbox(&mut self, message_id: &str) -> Result<RuntimeMailboxMessage, RuntimeError> {
        let current = self.mailbox.get(message_id).ok_or_else(|| {
            RuntimeError::NotFound(format!("Mailbox message {message_id} is not registered"))
        })?;
        match current.state {
            MailboxDeliveryState::Queued => return Ok(current.clone()),
            MailboxDeliveryState::Consumed => {
                return Err(RuntimeError::InvalidTransition(format!(
                    "Mailbox message {message_id} is already consumed"
                )));
            }
            MailboxDeliveryState::Claimed => {}
        }
        let mut message = current.clone();
        message.state = MailboxDeliveryState::Queued;
        message.claimed_at_ms = None;
        self.commit_mailbox_message(message, RuntimeEventKind::MailboxEnqueued)
    }

    fn reconcile_mailbox(
        &mut self,
        agent_id: &str,
        delivered: &RuntimeMailboxDeliveryReceipt,
    ) -> Result<(), RuntimeError> {
        let pending: Vec<(String, MailboxDeliveryState, bool)> = self
            .mailbox
            .values()
            .filter(|message| message.target_agent_id == agent_id)
            .map(|message| {
                (
                    message.message_id.clone(),
                    message.state,
                    delivered.message_ids.contains(&message.message_id)
                        || message
                            .notification_id
                            .as_ref()
                            .is_some_and(|notification_id| {
                                delivered.notification_ids.contains(notification_id)
                            }),
                )
            })
            .collect();
        for (message_id, state, delivered) in pending {
            if delivered {
                if state != MailboxDeliveryState::Consumed {
                    self.consume_mailbox(&message_id)?;
                }
            } else if state != MailboxDeliveryState::Queued {
                let mut message = self.mailbox.get(&message_id).cloned().ok_or_else(|| {
                    RuntimeError::NotFound(format!(
                        "Mailbox message {message_id} is not registered"
                    ))
                })?;
                message.state = MailboxDeliveryState::Queued;
                message.claimed_at_ms = None;
                message.consumed_at_ms = None;
                self.commit_mailbox_message(message, RuntimeEventKind::MailboxEnqueued)?;
            }
        }
        Ok(())
    }

    fn commit_execution_attempt(
        &mut self,
        attempt: ExecutionAttempt,
        kind: RuntimeEventKind,
    ) -> Result<ExecutionAttempt, RuntimeError> {
        let run = self.runs.get(&attempt.run_id).cloned().ok_or_else(|| {
            RuntimeError::NotFound(format!("Run {} is not registered", attempt.run_id))
        })?;
        let previous_sequence = self.next_sequence;
        let event = self.event(
            kind,
            Some(run.agent_id),
            Some(run.run_id),
            Some(&attempt),
            serde_json::json!({
                "connection_state": attempt.connection_state,
                "last_event_seq": attempt.last_event_seq,
                "released_at_ms": attempt.released_at_ms,
            }),
        );
        let result = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commit_runtime_attempt(&attempt, &event);
        if let Err(error) = result {
            self.next_sequence = previous_sequence;
            return Err(error);
        }
        self.attempts
            .insert(attempt.run_id.clone(), attempt.clone());
        self.publish(event);
        Ok(attempt)
    }

    pub(super) fn set_connection_state(
        &mut self,
        run_id: &str,
        state: RuntimeConnectionState,
    ) -> Result<ExecutionAttempt, RuntimeError> {
        if self
            .runs
            .get(run_id)
            .is_some_and(|run| run.backend_id == "local")
        {
            return Err(RuntimeError::InvalidTransition(
                "Local runs do not have a remote connection state".to_string(),
            ));
        }
        let mut attempt = self.attempts.get(run_id).cloned().ok_or_else(|| {
            RuntimeError::NotFound(format!("Run {run_id} has no execution attempt"))
        })?;
        if attempt.connection_state == state {
            return Ok(attempt);
        }
        attempt.connection_state = state;
        self.commit_execution_attempt(attempt, RuntimeEventKind::ConnectionChanged)
    }

    fn reconnect_run(
        &mut self,
        run_id: &str,
        cursor: u64,
    ) -> Result<ExecutionAttempt, RuntimeError> {
        let run = self
            .runs
            .get(run_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("Run {run_id} is not registered")))?;
        if run.status.is_terminal() {
            return Err(RuntimeError::InvalidTransition(format!(
                "Run {run_id} is already {}",
                run.status.as_str()
            )));
        }
        if run.backend_id == "local" {
            return Err(RuntimeError::InvalidTransition(
                "Local runs cannot reconnect".to_string(),
            ));
        }
        let current = self.attempts.get(run_id).cloned().ok_or_else(|| {
            RuntimeError::NotFound(format!("Run {run_id} has no execution attempt"))
        })?;
        let attempt = ExecutionAttempt {
            session_id: self.session_id.clone(),
            run_id: run_id.to_string(),
            attempt: current.attempt.saturating_add(1),
            backend_epoch: new_runtime_id("epoch"),
            connection_state: RuntimeConnectionState::Connected,
            last_event_seq: cursor,
            released_at_ms: None,
        };
        self.commit_execution_attempt(attempt, RuntimeEventKind::AttemptStarted)
    }

    fn record_run_activity(&mut self, run_id: &str, throttled: bool) -> Result<(), RuntimeError> {
        let run = self
            .runs
            .get(run_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("Run {run_id} is not registered")))?;
        if run.status != RuntimeRunStatus::Running {
            return Ok(());
        }
        let now = crate::model_io::now_ms();
        if throttled && now.saturating_sub(run.last_activity_at_ms) < 1_000 {
            return Ok(());
        }
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .update_runtime_activity(&self.session_id, run_id, now)?;
        if let Some(run) = self.runs.get_mut(run_id) {
            run.last_activity_at_ms = now;
        }
        Ok(())
    }

    fn check_watchdogs(&mut self, now: u64) {
        let expired: Vec<(String, super::RunWatchdogKind)> = self
            .runs
            .values()
            .flat_map(|run| {
                super::watchdog::due(run, now)
                    .into_iter()
                    .map(|kind| (run.run_id.clone(), kind))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (run_id, kind) in expired {
            let Some(current) = self.runs.get(&run_id).cloned() else {
                continue;
            };
            if !current.status.is_active() {
                continue;
            }
            if !current.watchdog_alerts.contains(&kind) {
                let Some(attempt) = self.attempts.get(&run_id).cloned() else {
                    continue;
                };
                let mut run = current.clone();
                run.watchdog_alerts.insert(kind);
                let previous_sequence = self.next_sequence;
                let event = self.event(
                    RuntimeEventKind::RunWatchdog,
                    Some(run.agent_id.clone()),
                    Some(run_id.clone()),
                    Some(&attempt),
                    serde_json::json!({
                        "watchdog": kind,
                        "action": run.watchdog_action,
                        "deadline_at_ms": run.deadline_at_ms,
                        "idle_timeout_ms": run.idle_timeout_ms,
                        "last_activity_at_ms": run.last_activity_at_ms,
                    }),
                );
                let result = self
                    .store
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .commit_runtime_run_update(&run, &event);
                if let Err(error) = result {
                    self.next_sequence = previous_sequence;
                    tracing::error!(%run_id, %error, "failed to persist runtime watchdog alert");
                    continue;
                }
                self.runs.insert(run_id.clone(), run);
                self.publish(event);
            }
            if current.watchdog_action == super::RunWatchdogAction::Stop
                && current.status != RuntimeRunStatus::Stopping
            {
                let Some(attempt) = self.attempts.get(&run_id).cloned() else {
                    continue;
                };
                if let Err(error) = self.request_stop(
                    &run_id,
                    attempt.attempt,
                    &attempt.backend_epoch,
                    kind.stop_reason().to_string(),
                ) {
                    tracing::warn!(%run_id, %error, "watchdog stop request failed");
                }
            }
        }
    }

    fn recover_interrupted_local_runs(&mut self) -> Result<(), RuntimeError> {
        let interrupted: Vec<(String, ExecutionAttempt)> = self
            .runs
            .values()
            .filter(|run| run.backend_id == "local" && run.status.is_active())
            .filter_map(|run| {
                self.attempts
                    .get(&run.run_id)
                    .cloned()
                    .map(|attempt| (run.run_id.clone(), attempt))
            })
            .collect();
        for (run_id, attempt) in interrupted {
            self.settle_run(
                &run_id,
                attempt.attempt,
                &attempt.backend_epoch,
                RuntimeRunStatus::Lost,
                Some("The previous runtime ended before this local run settled".to_string()),
                (0, 0, 0),
                0,
            )?;
        }
        let remote: Vec<String> = self
            .runs
            .values()
            .filter(|run| run.backend_id != "local" && run.status.is_active())
            .map(|run| run.run_id.clone())
            .collect();
        for run_id in remote {
            self.set_connection_state(&run_id, RuntimeConnectionState::Disconnected)?;
        }
        Ok(())
    }

    fn view(&self) -> RuntimeSnapshotView {
        RuntimeSnapshotView {
            session_id: self.session_id.clone(),
            agents: self.agents.values().cloned().collect(),
            runs: self.runs.values().cloned().collect(),
            attempts: self.attempts.values().cloned().collect(),
            mailbox: self.mailbox.values().cloned().collect(),
            latest_sequence: self.next_sequence,
            closed: self.closed,
        }
    }

    fn event(
        &mut self,
        kind: RuntimeEventKind,
        agent_id: Option<String>,
        run_id: Option<String>,
        attempt: Option<&ExecutionAttempt>,
        payload: serde_json::Value,
    ) -> RuntimeControlEvent {
        self.next_sequence += 1;
        RuntimeControlEvent {
            session_id: self.session_id.clone(),
            sequence: self.next_sequence,
            event_id: new_runtime_id("event"),
            kind,
            agent_id,
            run_id,
            attempt: attempt.map(|value| value.attempt),
            backend_epoch: attempt.map(|value| value.backend_epoch.clone()),
            payload,
            created_at_ms: crate::model_io::now_ms(),
        }
    }

    fn register_agent(&mut self, mut agent: RuntimeAgent) -> Result<RuntimeAgent, RuntimeError> {
        if agent.session_id != self.session_id {
            return Err(RuntimeError::Conflict(format!(
                "Agent {} belongs to session {}, not {}",
                agent.agent_id, agent.session_id, self.session_id
            )));
        }
        if let Some(existing) = self.agents.get(&agent.agent_id) {
            if existing.closed_at_ms.is_some() {
                return Err(RuntimeError::InvalidTransition(format!(
                    "Agent {} is closed",
                    agent.agent_id
                )));
            }
            let unchanged = existing.parent_agent_id == agent.parent_agent_id
                && existing.depth == agent.depth
                && existing.profile == agent.profile
                && existing.conversation_path == agent.conversation_path;
            if unchanged {
                return Ok(existing.clone());
            }
            agent.created_at_ms = existing.created_at_ms;
        }
        if let Some(parent) = &agent.parent_agent_id {
            let Some(parent_agent) = self.agents.get(parent) else {
                return Err(RuntimeError::NotFound(format!(
                    "Parent agent {parent} is not registered"
                )));
            };
            if parent_agent.closed_at_ms.is_some() {
                return Err(RuntimeError::InvalidTransition(format!(
                    "Parent agent {parent} is closed"
                )));
            }
        } else if agent.depth != 0 {
            return Err(RuntimeError::Conflict(
                "A root agent must have delegation depth zero".to_string(),
            ));
        }
        let event = self.event(
            RuntimeEventKind::AgentRegistered,
            Some(agent.agent_id.clone()),
            None,
            None,
            serde_json::json!({
                "parent_agent_id": agent.parent_agent_id,
                "depth": agent.depth,
                "profile": agent.profile,
            }),
        );
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commit_runtime_agent(&agent, &event)?;
        self.agents.insert(agent.agent_id.clone(), agent.clone());
        self.publish(event);
        Ok(agent)
    }

    fn start_run(
        &mut self,
        request: StartRun,
    ) -> Result<(RuntimeRun, ExecutionAttempt), RuntimeError> {
        if request.watchdog.action == super::RunWatchdogAction::Stop
            && request.backend_id != "local"
        {
            return Err(RuntimeError::InvalidTransition(
                "automatic watchdog stopping currently requires a local execution".to_string(),
            ));
        }
        if let Some(key) = &request.idempotency_key
            && let Some(existing) = self
                .runs
                .values()
                .find(|run| run.idempotency_key.as_ref() == Some(key))
        {
            if existing.agent_id != request.agent_id {
                return Err(RuntimeError::Conflict(format!(
                    "Idempotency key {key} is already bound to agent {}",
                    existing.agent_id
                )));
            }
            let attempt = self
                .attempts
                .get(&existing.run_id)
                .cloned()
                .ok_or_else(|| {
                    RuntimeError::Store(format!("Run {} has no execution attempt", existing.run_id))
                })?;
            return Ok((existing.clone(), attempt));
        }
        let agent = self.agents.get(&request.agent_id).ok_or_else(|| {
            RuntimeError::NotFound(format!("Agent {} is not registered", request.agent_id))
        })?;
        if agent.closed_at_ms.is_some() {
            return Err(RuntimeError::InvalidTransition(format!(
                "Agent {} is closed",
                request.agent_id
            )));
        }
        if self
            .runs
            .values()
            .any(|run| run.agent_id == request.agent_id && run.status.is_active())
        {
            return Err(RuntimeError::Conflict(format!(
                "Agent {} already has an active run",
                request.agent_id
            )));
        }
        let run_id = request
            .run_id
            .clone()
            .unwrap_or_else(|| new_runtime_id("run"));
        let now = crate::model_io::now_ms();
        let run = RuntimeRun {
            session_id: self.session_id.clone(),
            run_id: run_id.clone(),
            agent_id: request.agent_id,
            backend_id: request.backend_id,
            initiator: request.initiator,
            idempotency_key: request.idempotency_key,
            causation_id: request.causation_id,
            correlation_id: request.correlation_id,
            status: if request.queued {
                RuntimeRunStatus::Queued
            } else {
                RuntimeRunStatus::Running
            },
            stop_reason: None,
            usage: (0, 0, 0),
            turns: 0,
            started_at_ms: now,
            ended_at_ms: None,
            deadline_at_ms: request
                .watchdog
                .deadline
                .map(|deadline| now.saturating_add(super::watchdog::duration_ms(deadline))),
            idle_timeout_ms: request
                .watchdog
                .idle_timeout
                .map(super::watchdog::duration_ms),
            last_activity_at_ms: now,
            watchdog_action: request.watchdog.action,
            watchdog_alerts: Default::default(),
        };
        let attempt = ExecutionAttempt {
            session_id: self.session_id.clone(),
            run_id: run_id.clone(),
            attempt: 1,
            backend_epoch: new_runtime_id("epoch"),
            connection_state: if run.backend_id == "local" {
                RuntimeConnectionState::NotApplicable
            } else {
                RuntimeConnectionState::Connected
            },
            last_event_seq: 0,
            released_at_ms: None,
        };
        let event = self.event(
            RuntimeEventKind::RunStarted,
            Some(run.agent_id.clone()),
            Some(run_id.clone()),
            Some(&attempt),
            serde_json::json!({
                "backend_id": run.backend_id,
                "initiator": run.initiator,
                "idempotency_key": run.idempotency_key,
                "causation_id": run.causation_id,
                "correlation_id": run.correlation_id,
                "deadline_at_ms": run.deadline_at_ms,
                "idle_timeout_ms": run.idle_timeout_ms,
                "watchdog_action": run.watchdog_action,
            }),
        );
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commit_runtime_run(&run, &attempt, &event)?;
        self.runs.insert(run_id.clone(), run.clone());
        self.attempts.insert(run_id, attempt.clone());
        self.publish(event);
        Ok((run, attempt))
    }

    fn mark_run_running(&mut self, run_id: &str) -> Result<RuntimeRun, RuntimeError> {
        let current = self
            .runs
            .get(run_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("Run {run_id} is not registered")))?;
        if current.status == RuntimeRunStatus::Running {
            return Ok(current.clone());
        }
        if current.status != RuntimeRunStatus::Queued {
            return Err(RuntimeError::InvalidTransition(format!(
                "Run {run_id} cannot start from {}",
                current.status.as_str()
            )));
        }
        let execution = self.attempts.get(run_id).cloned().ok_or_else(|| {
            RuntimeError::NotFound(format!("Run {run_id} has no execution attempt"))
        })?;
        let mut run = current.clone();
        run.status = RuntimeRunStatus::Running;
        run.last_activity_at_ms = crate::model_io::now_ms();
        let previous_sequence = self.next_sequence;
        let event = self.event(
            RuntimeEventKind::RunStarted,
            Some(run.agent_id.clone()),
            Some(run_id.to_string()),
            Some(&execution),
            serde_json::json!({ "status": RuntimeRunStatus::Running }),
        );
        let result = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commit_runtime_run_update(&run, &event);
        if let Err(error) = result {
            self.next_sequence = previous_sequence;
            return Err(error);
        }
        self.runs.insert(run_id.to_string(), run.clone());
        self.publish(event);
        Ok(run)
    }

    pub(super) fn run_is_active(&self, run_id: &str) -> bool {
        self.runs
            .get(run_id)
            .is_some_and(|run| run.status.is_active())
    }

    pub(super) fn execution_identity(
        &self,
        run_id: &str,
    ) -> Result<crate::agent::AgentExecutionIdentity, RuntimeError> {
        let run = self
            .runs
            .get(run_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("Run {run_id} is not registered")))?;
        let attempt = self.attempts.get(run_id).ok_or_else(|| {
            RuntimeError::NotFound(format!("Run {run_id} has no execution attempt"))
        })?;
        Ok(crate::agent::AgentExecutionIdentity {
            run: crate::agent::AgentRunIdentity {
                session_id: run.session_id.clone(),
                agent_id: run.agent_id.clone(),
                run_id: run.run_id.clone(),
            },
            attempt: attempt.attempt,
            backend_epoch: attempt.backend_epoch.clone(),
        })
    }

    fn current_attempt(
        &self,
        run_id: &str,
        attempt: u64,
        epoch: &str,
    ) -> Result<ExecutionAttempt, RuntimeError> {
        let current = self.attempts.get(run_id).ok_or_else(|| {
            RuntimeError::NotFound(format!("Run {run_id} has no execution attempt"))
        })?;
        if current.attempt != attempt || current.backend_epoch != epoch {
            return Err(RuntimeError::StaleAttempt {
                run_id: run_id.to_string(),
                attempt,
                backend_epoch: epoch.to_string(),
            });
        }
        Ok(current.clone())
    }

    pub(super) fn release_execution(
        &mut self,
        run_id: &str,
        attempt: u64,
        backend_epoch: &str,
    ) -> Result<ExecutionAttempt, RuntimeError> {
        let execution = self.current_attempt(run_id, attempt, backend_epoch)?;
        let run = self
            .runs
            .get(run_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("Run {run_id} is not registered")))?;
        if !run.status.is_terminal() {
            return Err(RuntimeError::InvalidTransition(format!(
                "Run {run_id} must settle before its execution is released"
            )));
        }
        if execution.released_at_ms.is_some() {
            return Ok(execution);
        }
        let mut released = execution;
        released.released_at_ms = Some(crate::model_io::now_ms());
        self.commit_execution_attempt(released, RuntimeEventKind::ExecutionReleased)
    }

    pub(super) fn request_stop(
        &mut self,
        run_id: &str,
        attempt: u64,
        backend_epoch: &str,
        reason: String,
    ) -> Result<RuntimeRun, RuntimeError> {
        let execution = self.current_attempt(run_id, attempt, backend_epoch)?;
        let current = self
            .runs
            .get(run_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("Run {run_id} is not registered")))?;
        if current.status == RuntimeRunStatus::Stopping {
            return Ok(current.clone());
        }
        if current.status.is_terminal() {
            return Err(RuntimeError::InvalidTransition(format!(
                "Run {run_id} is already {}",
                current.status.as_str()
            )));
        }
        let mut run = current.clone();
        run.status = RuntimeRunStatus::Stopping;
        run.stop_reason = Some(reason.clone());
        let previous_sequence = self.next_sequence;
        let event = self.event(
            RuntimeEventKind::RunStopping,
            Some(run.agent_id.clone()),
            Some(run_id.to_string()),
            Some(&execution),
            serde_json::json!({ "reason": reason }),
        );
        let result = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commit_runtime_run_update(&run, &event);
        if let Err(error) = result {
            self.next_sequence = previous_sequence;
            return Err(error);
        }
        self.runs.insert(run_id.to_string(), run.clone());
        self.publish(event);
        if let Some(cancel) = self.stop_hooks.get(run_id) {
            cancel.cancel();
        }
        Ok(run)
    }

    pub(super) fn settle_run(
        &mut self,
        run_id: &str,
        attempt: u64,
        backend_epoch: &str,
        status: RuntimeRunStatus,
        stop_reason: Option<String>,
        usage: (u64, u64, u64),
        turns: usize,
    ) -> Result<RuntimeRun, RuntimeError> {
        if !status.is_terminal() {
            return Err(RuntimeError::InvalidTransition(format!(
                "{} is not a terminal run status",
                status.as_str()
            )));
        }
        let execution = self.current_attempt(run_id, attempt, backend_epoch)?;
        let current = self
            .runs
            .get(run_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("Run {run_id} is not registered")))?;
        if current.status.is_terminal() {
            if current.status == status
                && current.stop_reason == stop_reason
                && current.usage == usage
                && current.turns == turns
            {
                return Ok(current.clone());
            }
            return Err(RuntimeError::Conflict(format!(
                "Run {run_id} is already settled as {}",
                current.status.as_str()
            )));
        }
        let mut run = current.clone();
        run.status = status;
        run.stop_reason = stop_reason.clone();
        run.usage = usage;
        run.turns = turns;
        run.ended_at_ms = Some(crate::model_io::now_ms());
        let previous_sequence = self.next_sequence;
        let event = self.event(
            RuntimeEventKind::RunSettled,
            Some(run.agent_id.clone()),
            Some(run_id.to_string()),
            Some(&execution),
            serde_json::json!({
                "status": status,
                "stop_reason": stop_reason,
                "usage": usage,
                "turns": turns,
            }),
        );
        let result = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commit_runtime_run_update(&run, &event);
        if let Err(error) = result {
            self.next_sequence = previous_sequence;
            return Err(error);
        }
        self.runs.insert(run_id.to_string(), run.clone());
        self.stop_hooks.remove(run_id);
        self.publish(event);
        Ok(run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RemoteBackendFactory;

    impl crate::agent::AgentBackendFactory for RemoteBackendFactory {
        type Context = ();

        fn capabilities(
            &self,
        ) -> std::collections::BTreeSet<crate::agent::AgentExecutionCapability> {
            [
                crate::agent::AgentExecutionCapability::Reconnect,
                crate::agent::AgentExecutionCapability::Stop,
            ]
            .into_iter()
            .collect()
        }

        fn create(
            &self,
            _context: (),
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<Box<dyn crate::agent::AgentExecutionBackend>, crate::agent::AgentBackendError>,
        > {
            Box::pin(async {
                Err(crate::agent::AgentBackendError::Failed(
                    "control-plane test does not execute a backend".into(),
                ))
            })
        }
    }

    fn store(name: &str) -> (std::path::PathBuf, Arc<Mutex<Store>>) {
        let root = std::env::temp_dir().join(format!(
            "pig-agent-runtime-{name}-{}-{}",
            std::process::id(),
            crate::model_io::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = Arc::new(Mutex::new(Store::open(&root).expect("runtime store")));
        (root, store)
    }

    fn agent(session: &str, id: &str, parent: Option<&str>) -> RuntimeAgent {
        RuntimeAgent {
            session_id: session.to_string(),
            agent_id: id.to_string(),
            parent_agent_id: parent.map(str::to_string),
            depth: u32::from(parent.is_some()),
            profile: if parent.is_some() { "explore" } else { "main" }.to_string(),
            conversation_path: None,
            created_at_ms: crate::model_io::now_ms(),
            closed_at_ms: None,
        }
    }

    fn start(agent_id: &str, key: &str) -> StartRun {
        StartRun {
            run_id: None,
            agent_id: agent_id.to_string(),
            backend_id: "local".to_string(),
            required_capabilities: Default::default(),
            initiator: AgentInitiator::User {
                request_id: Some(key.to_string()),
            },
            idempotency_key: Some(key.to_string()),
            causation_id: Some("cause-1".to_string()),
            correlation_id: Some("correlation-1".to_string()),
            queued: false,
            watchdog: Default::default(),
        }
    }

    fn watchdog(
        deadline_ms: Option<u64>,
        idle_timeout_ms: Option<u64>,
        action: super::super::RunWatchdogAction,
    ) -> super::super::RunWatchdogPolicy {
        super::super::RunWatchdogPolicy {
            deadline: deadline_ms.map(std::time::Duration::from_millis),
            idle_timeout: idle_timeout_ms.map(std::time::Duration::from_millis),
            action,
        }
    }

    #[tokio::test]
    async fn runtime_commits_ordered_events_and_restores_run_history() {
        let (root, store) = store("restore");
        let runtime = AgentRuntime::spawn("s1".to_string(), store.clone()).expect("runtime");
        let mut events = runtime.subscribe();
        runtime
            .register_agent(agent("s1", "s1", None))
            .await
            .expect("root");
        runtime
            .register_agent(agent("s1", "a1", Some("s1")))
            .await
            .expect("child");
        let (run, attempt) = runtime
            .start_run(start("a1", "spawn-1"))
            .await
            .expect("start");
        let (duplicate, duplicate_attempt) = runtime
            .start_run(start("a1", "spawn-1"))
            .await
            .expect("idempotent");
        assert_eq!(duplicate.run_id, run.run_id);
        assert_eq!(duplicate_attempt.backend_epoch, attempt.backend_epoch);
        assert!(matches!(
            runtime
                .request_stop(
                    run.run_id.clone(),
                    0,
                    "old".to_string(),
                    "stale".to_string()
                )
                .await,
            Err(RuntimeError::StaleAttempt { .. })
        ));
        runtime
            .request_stop(
                run.run_id.clone(),
                attempt.attempt,
                attempt.backend_epoch.clone(),
                "user stop".to_string(),
            )
            .await
            .expect("stopping");
        runtime
            .settle_run(
                run.run_id.clone(),
                attempt.attempt,
                attempt.backend_epoch.clone(),
                RuntimeRunStatus::Cancelled,
                Some("user stop".to_string()),
                (7, 3, 2),
            )
            .await
            .expect("settle");
        let first = runtime
            .settle_run(
                run.run_id.clone(),
                attempt.attempt,
                attempt.backend_epoch.clone(),
                RuntimeRunStatus::Cancelled,
                Some("user stop".to_string()),
                (7, 3, 2),
            )
            .await
            .expect("idempotent settle");
        assert!(first.ended_at_ms.is_some());

        let mut received = Vec::new();
        for _ in 0..5 {
            received.push(events.recv().await.expect("event"));
        }
        assert_eq!(
            received
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(received[2].kind, RuntimeEventKind::RunStarted);
        assert_eq!(received[3].kind, RuntimeEventKind::RunStopping);
        assert_eq!(received[4].kind, RuntimeEventKind::RunSettled);
        runtime.shutdown().await.expect("shutdown");

        let restored = AgentRuntime::spawn("s1".to_string(), store).expect("restored runtime");
        let snapshot = restored.snapshot().await.expect("snapshot");
        assert_eq!(snapshot.agents.len(), 2);
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.runs[0].status, RuntimeRunStatus::Cancelled);
        assert_eq!(snapshot.runs[0].usage, (7, 3, 2));
        assert_eq!(snapshot.latest_sequence, 5);
        let (_, second_attempt) = restored
            .start_run(start("a1", "spawn-2"))
            .await
            .expect("second run");
        assert_eq!(second_attempt.attempt, 1);
        assert_eq!(
            restored.snapshot().await.expect("snapshot").latest_sequence,
            6
        );
        restored.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn remote_reconnect_advances_attempt_and_rejects_stale_epoch() {
        let (root, store) = store("remote-reconnect");
        let backends = Arc::new(crate::agent::AgentBackendRegistry::with_local());
        backends
            .register("ipc", RemoteBackendFactory)
            .expect("remote backend");
        let runtime =
            AgentRuntime::spawn_with_backends("s6".to_string(), store.clone(), backends.clone())
                .expect("runtime");
        runtime
            .register_agent(agent("s6", "s6", None))
            .await
            .expect("root");
        let mut request = start("s6", "ipc-run");
        request.backend_id = "ipc".to_string();
        let (run, first) = runtime.start_run(request).await.expect("ipc run");
        runtime
            .disconnect_run_blocking(run.run_id.clone())
            .expect("disconnect");
        runtime.shutdown().await.expect("shutdown");

        let restored =
            AgentRuntime::spawn_with_backends("s6".to_string(), store, backends).expect("restored");
        let snapshot = restored.snapshot().await.expect("snapshot");
        assert_eq!(snapshot.runs[0].status, RuntimeRunStatus::Running);
        assert_eq!(
            snapshot.attempts[0].connection_state,
            RuntimeConnectionState::Disconnected
        );
        let second = restored
            .reconnect_run_blocking(run.run_id.clone(), 7)
            .expect("reconnect");
        assert_eq!(second.attempt, first.attempt + 1);
        assert_ne!(second.backend_epoch, first.backend_epoch);
        assert_eq!(second.last_event_seq, 7);
        assert!(matches!(
            restored
                .settle_run(
                    run.run_id.clone(),
                    first.attempt,
                    first.backend_epoch,
                    RuntimeRunStatus::Completed,
                    Some("stale".to_string()),
                    (0, 0, 0),
                )
                .await,
            Err(RuntimeError::StaleAttempt { .. })
        ));
        restored
            .settle_run(
                run.run_id,
                second.attempt,
                second.backend_epoch,
                RuntimeRunStatus::Completed,
                Some("completed".to_string()),
                (0, 0, 0),
            )
            .await
            .expect("current settlement");
        restored.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runtime_publishes_committed_events_to_registered_projections() {
        struct StatusProjection;
        impl super::super::RuntimeProjectionDefinition for StatusProjection {
            fn key(&self) -> &str {
                "test.status"
            }

            fn version(&self) -> u32 {
                1
            }

            fn initial(&self, _snapshot: &RuntimeSnapshotView) -> serde_json::Value {
                serde_json::json!("idle")
            }

            fn apply(&self, state: &mut serde_json::Value, event: &RuntimeControlEvent) {
                if event.kind == RuntimeEventKind::RunStarted {
                    *state = serde_json::json!("running");
                } else if event.kind == RuntimeEventKind::RunSettled {
                    *state = serde_json::json!("settled");
                }
            }
        }

        let (root, store) = store("projection");
        let runtime = AgentRuntime::spawn("s5".to_string(), store).expect("runtime");
        runtime
            .register_agent(agent("s5", "s5", None))
            .await
            .expect("root");
        runtime
            .register_projection_blocking(Box::new(StatusProjection))
            .expect("register projection");
        let (run, attempt) = runtime
            .start_run(start("s5", "projected"))
            .await
            .expect("run");
        assert_eq!(
            runtime
                .projection_snapshot_blocking()
                .expect("projection")
                .values["test.status"]
                .value,
            serde_json::json!("running")
        );
        runtime
            .settle_run(
                run.run_id,
                attempt.attempt,
                attempt.backend_epoch,
                RuntimeRunStatus::Completed,
                Some("completed".to_string()),
                (0, 0, 0),
            )
            .await
            .expect("settle");
        assert_eq!(
            runtime
                .projection_snapshot_blocking()
                .expect("projection")
                .values["test.status"]
                .value,
            serde_json::json!("settled")
        );
        runtime.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn mailbox_reconciles_claimed_and_jsonl_delivered_messages() {
        let (root, store) = store("mailbox");
        let runtime = AgentRuntime::spawn("s4".to_string(), store.clone()).expect("runtime");
        runtime
            .register_agent(agent("s4", "s4", None))
            .await
            .expect("root");
        let message = RuntimeMailboxMessage::new(
            "s4".to_string(),
            "mail-1".to_string(),
            "s4".to_string(),
            None,
            MailboxMessageKind::Notification,
            serde_json::json!({ "text": "done", "notification": null }),
            Some("notification-1".to_string()),
        );
        runtime
            .enqueue_mailbox_blocking(message.clone())
            .expect("enqueue");
        assert_eq!(
            runtime
                .claim_mailbox_blocking("s4".to_string())
                .expect("claim")[0]
                .state,
            MailboxDeliveryState::Claimed
        );
        runtime.shutdown().await.expect("shutdown");

        let restored = AgentRuntime::spawn("s4".to_string(), store.clone()).expect("restored");
        restored
            .reconcile_mailbox_blocking("s4".to_string(), Default::default())
            .expect("requeue uncommitted claim");
        assert_eq!(
            restored
                .requeue_mailbox_blocking("mail-1".to_string())
                .expect("idempotent requeue")
                .state,
            MailboxDeliveryState::Queued
        );
        restored
            .claim_mailbox_blocking("s4".to_string())
            .expect("second claim");
        restored.shutdown().await.expect("shutdown");

        let delivered = AgentRuntime::spawn("s4".to_string(), store).expect("delivered restore");
        delivered
            .reconcile_mailbox_blocking(
                "s4".to_string(),
                RuntimeMailboxDeliveryReceipt {
                    message_ids: ["mail-1".to_string()].into_iter().collect(),
                    notification_ids: Default::default(),
                },
            )
            .expect("consume durable delivery");
        assert!(
            delivered
                .claim_mailbox_blocking("s4".to_string())
                .expect("claim")
                .is_empty()
        );
        assert_eq!(
            delivered
                .enqueue_mailbox_blocking(message)
                .expect("idempotent enqueue")
                .state,
            MailboxDeliveryState::Consumed
        );
        delivered.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn mailbox_repairs_missing_delivery_and_honors_task_wait_receipts() {
        let (root, store) = store("mailbox-reverse");
        let runtime = AgentRuntime::spawn("s10".to_string(), store).expect("runtime");
        runtime
            .register_agent(agent("s10", "s10", None))
            .await
            .expect("root");
        let message = RuntimeMailboxMessage::new(
            "s10".to_string(),
            "mail-task-wait".to_string(),
            "s10".to_string(),
            None,
            MailboxMessageKind::Notification,
            serde_json::json!({ "text": "done", "notification": null }),
            Some("notification-task-wait".to_string()),
        );
        runtime.enqueue_mailbox_blocking(message).expect("enqueue");
        runtime
            .consume_mailbox_blocking("mail-task-wait".to_string())
            .expect("consume without JSONL acknowledgment");
        runtime
            .reconcile_mailbox_blocking("s10".to_string(), Default::default())
            .expect("repair missing JSONL");
        let repaired = runtime
            .snapshot_blocking()
            .expect("snapshot")
            .mailbox
            .remove(0);
        assert_eq!(repaired.state, MailboxDeliveryState::Queued);
        assert_eq!(repaired.consumed_at_ms, None);

        runtime
            .consume_mailbox_blocking("mail-task-wait".to_string())
            .expect("TaskWait consumption");
        runtime
            .reconcile_mailbox_blocking(
                "s10".to_string(),
                RuntimeMailboxDeliveryReceipt {
                    message_ids: Default::default(),
                    notification_ids: ["notification-task-wait".to_string()].into_iter().collect(),
                },
            )
            .expect("honor TaskWait receipt");
        assert!(
            runtime
                .claim_mailbox_blocking("s10".to_string())
                .expect("claim")
                .is_empty()
        );
        runtime.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reopen_marks_unsettled_local_runs_lost() {
        let (root, store) = store("lost");
        let runtime = AgentRuntime::spawn("s3".to_string(), store.clone()).expect("runtime");
        runtime
            .register_agent(agent("s3", "s3", None))
            .await
            .expect("root");
        let (run, _) = runtime
            .start_run(start("s3", "interrupted"))
            .await
            .expect("run");
        runtime.shutdown().await.expect("shutdown");
        drop(runtime);

        let restored = AgentRuntime::spawn("s3".to_string(), store).expect("restored");
        let snapshot = restored.snapshot().await.expect("snapshot");
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.runs[0].run_id, run.run_id);
        assert_eq!(snapshot.runs[0].status, RuntimeRunStatus::Lost);
        assert_eq!(snapshot.latest_sequence, 3);
        restored.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn execution_release_is_terminal_fenced_persistent_and_idempotent() {
        let (root, store) = store("execution-release");
        let runtime = AgentRuntime::spawn("s-release".to_string(), store.clone()).expect("runtime");
        runtime
            .register_agent(agent("s-release", "s-release", None))
            .await
            .expect("root");
        let (run, attempt) = runtime
            .start_run(start("s-release", "release"))
            .await
            .expect("run");
        assert!(matches!(
            runtime.release_execution_blocking(
                run.run_id.clone(),
                attempt.attempt,
                attempt.backend_epoch.clone(),
            ),
            Err(RuntimeError::InvalidTransition(_))
        ));
        runtime
            .settle_run(
                run.run_id.clone(),
                attempt.attempt,
                attempt.backend_epoch.clone(),
                RuntimeRunStatus::Completed,
                Some("completed".into()),
                (0, 0, 0),
            )
            .await
            .expect("settle");
        let released = runtime
            .release_execution_blocking(run.run_id.clone(), attempt.attempt, attempt.backend_epoch)
            .expect("release");
        assert!(released.released_at_ms.is_some());
        let sequence = runtime
            .snapshot_blocking()
            .expect("snapshot")
            .latest_sequence;
        assert_eq!(
            runtime
                .release_execution_blocking(run.run_id, attempt.attempt, released.backend_epoch)
                .expect("idempotent release")
                .released_at_ms,
            released.released_at_ms
        );
        assert_eq!(
            runtime
                .snapshot_blocking()
                .expect("snapshot")
                .latest_sequence,
            sequence
        );
        runtime.shutdown().await.expect("shutdown");

        let restored = AgentRuntime::spawn("s-release".to_string(), store).expect("restored");
        assert_eq!(
            restored.snapshot_blocking().expect("snapshot").attempts[0].released_at_ms,
            released.released_at_ms
        );
        restored.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn watchdog_observes_deadline_and_idle_once() {
        let (root, store) = store("watchdog-observe");
        let runtime = AgentRuntime::spawn("s7".to_string(), store).expect("runtime");
        runtime
            .register_agent(agent("s7", "s7", None))
            .await
            .expect("root");
        let mut request = start("s7", "watchdog-observe");
        request.watchdog = watchdog(Some(15), Some(10), super::super::RunWatchdogAction::Observe);
        let (run, _) = runtime.start_run(request).await.expect("run");
        let mut events = runtime.subscribe();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        runtime.check_watchdogs_blocking().expect("check watchdogs");
        runtime.check_watchdogs_blocking().expect("repeat check");
        let snapshot = runtime.snapshot_blocking().expect("snapshot");
        let current = snapshot
            .runs
            .iter()
            .find(|candidate| candidate.run_id == run.run_id)
            .expect("run");
        assert_eq!(current.status, RuntimeRunStatus::Running);
        assert_eq!(
            current.watchdog_alerts,
            [
                super::super::RunWatchdogKind::Deadline,
                super::super::RunWatchdogKind::Idle,
            ]
            .into_iter()
            .collect()
        );
        let mut alerts = 0;
        while let Ok(event) = events.try_recv() {
            alerts += usize::from(event.kind == RuntimeEventKind::RunWatchdog);
        }
        assert_eq!(alerts, 2);
        runtime.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn watchdog_activity_delays_idle_and_survives_restart() {
        let (root, store) = store("watchdog-heartbeat");
        let runtime = AgentRuntime::spawn("s8".to_string(), store.clone()).expect("runtime");
        runtime.disable_watchdog_ticks_blocking();
        runtime
            .register_agent(agent("s8", "s8", None))
            .await
            .expect("root");
        let mut request = start("s8", "watchdog-heartbeat");
        request.watchdog = watchdog(None, Some(60), super::super::RunWatchdogAction::Observe);
        let (run, _) = runtime.start_run(request).await.expect("run");
        let started_at = runtime.snapshot_blocking().expect("snapshot").runs[0].last_activity_at_ms;
        runtime.check_watchdogs_at_blocking(started_at + 30);
        runtime
            .record_run_activity_blocking(run.run_id.clone())
            .expect("activity");
        let activity = runtime.snapshot_blocking().expect("snapshot").runs[0].last_activity_at_ms;
        runtime.check_watchdogs_at_blocking(activity + 59);
        assert!(
            runtime.snapshot_blocking().expect("snapshot").runs[0]
                .watchdog_alerts
                .is_empty()
        );
        runtime.shutdown().await.expect("shutdown");

        let restored = AgentRuntime::spawn("s8".to_string(), store).expect("restored");
        let current = &restored.snapshot_blocking().expect("snapshot").runs[0];
        assert_eq!(current.status, RuntimeRunStatus::Lost);
        assert_eq!(current.last_activity_at_ms, activity);
        assert!(current.watchdog_alerts.is_empty());
        restored.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn watchdog_stop_cancels_local_execution() {
        let (root, store) = store("watchdog-stop");
        let runtime = AgentRuntime::spawn("s9".to_string(), store).expect("runtime");
        runtime
            .register_agent(agent("s9", "s9", None))
            .await
            .expect("root");
        let mut request = start("s9", "watchdog-stop");
        request.watchdog = watchdog(Some(20), None, super::super::RunWatchdogAction::Stop);
        let (run, attempt) = runtime.start_run(request).await.expect("run");
        let cancel = tokio_util::sync::CancellationToken::new();
        runtime
            .register_execution_cancel_blocking(run.run_id.clone(), cancel.clone())
            .expect("register cancel");
        tokio::time::timeout(std::time::Duration::from_secs(2), cancel.cancelled())
            .await
            .expect("watchdog cancellation timeout");
        let current = runtime
            .snapshot_blocking()
            .expect("snapshot")
            .runs
            .remove(0);
        assert_eq!(current.status, RuntimeRunStatus::Stopping);
        assert_eq!(
            current.stop_reason.as_deref(),
            Some("run deadline exceeded")
        );
        runtime
            .settle_run(
                run.run_id,
                attempt.attempt,
                attempt.backend_epoch,
                RuntimeRunStatus::Cancelled,
                Some("run deadline exceeded".to_string()),
                (0, 0, 0),
            )
            .await
            .expect("settle");
        runtime.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shutdown_rejects_later_commands() {
        let (root, store) = store("closed");
        let runtime = AgentRuntime::spawn("s2".to_string(), store).expect("runtime");
        runtime.shutdown().await.expect("shutdown");
        assert!(matches!(
            runtime.register_agent(agent("s2", "s2", None)).await,
            Err(RuntimeError::ChannelClosed | RuntimeError::Closed)
        ));
        let _ = std::fs::remove_dir_all(root);
    }
}
