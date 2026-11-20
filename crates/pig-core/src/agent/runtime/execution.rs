use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::agent::{
    AgentBackendError, AgentExecutionHandle, AgentExecutionIdentity, AgentExecutionInput,
    AgentExecutionReceipt, AgentExecutionRelease, AgentExecutionStop, AgentExecutionUpdate,
};

use super::{RuntimeError, RuntimeRunStatus};

struct ExecutionEntry {
    handle: Box<dyn AgentExecutionHandle>,
    detached: bool,
}

#[derive(Default)]
struct TableState {
    entries: BTreeMap<String, ExecutionEntry>,
    busy: BTreeSet<String>,
}

#[derive(Clone, Default)]
pub(super) struct ExecutionTable {
    state: Arc<Mutex<TableState>>,
}

impl ExecutionTable {
    fn lock(&self) -> MutexGuard<'_, TableState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn register(
        &self,
        state: &super::control::RuntimeState,
        handle: Box<dyn AgentExecutionHandle>,
    ) -> Result<AgentExecutionIdentity, (RuntimeError, Box<dyn AgentExecutionHandle>)> {
        let identity = handle.identity().clone();
        let run_id = identity.run.run_id.clone();
        let current = match state.execution_identity(&run_id) {
            Ok(current) => current,
            Err(error) => return Err((error, handle)),
        };
        if let Err(error) = fence(&current, &identity) {
            return Err((error, handle));
        }
        if !state.run_is_active(&run_id) {
            return Err((
                RuntimeError::InvalidTransition(format!("Run {run_id} is not active")),
                handle,
            ));
        }
        let mut table = self.lock();
        if table.busy.contains(&run_id) {
            return Err((
                RuntimeError::Conflict(format!(
                    "Run {run_id} has an in-flight execution operation"
                )),
                handle,
            ));
        }
        if let Some(existing) = table.entries.get(&run_id) {
            if existing.handle.identity() == &identity {
                return Err((
                    RuntimeError::Conflict(format!("Run {run_id} already has an execution handle")),
                    handle,
                ));
            }
            if !existing.detached {
                return Err((
                    RuntimeError::Conflict(format!(
                        "Run {run_id} must detach its old Attempt before replacement"
                    )),
                    handle,
                ));
            }
        }
        table.entries.insert(
            run_id,
            ExecutionEntry {
                handle,
                detached: false,
            },
        );
        Ok(identity)
    }

    fn take(
        &self,
        state: &super::control::RuntimeState,
        run_id: &str,
    ) -> Result<ExecutionEntry, RuntimeError> {
        let current = state.execution_identity(run_id)?;
        let mut table = self.lock();
        if let Some(entry) = table.entries.get(run_id) {
            fence(&current, entry.handle.identity())?;
        }
        let Some(entry) = table.entries.remove(run_id) else {
            if table.busy.contains(run_id) {
                return Err(RuntimeError::Conflict(format!(
                    "Run {run_id} has an in-flight execution operation"
                )));
            }
            return Err(RuntimeError::NotFound(format!(
                "Run {run_id} has no registered execution handle"
            )));
        };
        table.busy.insert(run_id.to_string());
        Ok(entry)
    }

    fn put(
        &self,
        state: &super::control::RuntimeState,
        run_id: &str,
        entry: ExecutionEntry,
    ) -> Result<(), RuntimeError> {
        let current = state.execution_identity(run_id)?;
        fence(&current, entry.handle.identity())?;
        let mut table = self.lock();
        if table.entries.contains_key(run_id) {
            return Err(RuntimeError::Conflict(format!(
                "Run {run_id} already has a replacement execution handle"
            )));
        }
        table.entries.insert(run_id.to_string(), entry);
        table.busy.remove(run_id);
        Ok(())
    }

    fn finish(&self, run_id: &str) {
        self.lock().busy.remove(run_id);
    }

    fn has_resources(&self, run_id: &str) -> bool {
        let table = self.lock();
        table.entries.contains_key(run_id) || table.busy.contains(run_id)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().entries.len()
    }
}

fn fence(
    current: &AgentExecutionIdentity,
    actual: &AgentExecutionIdentity,
) -> Result<(), RuntimeError> {
    if current == actual {
        return Ok(());
    }
    Err(RuntimeError::StaleAttempt {
        run_id: actual.run.run_id.clone(),
        attempt: actual.attempt,
        backend_epoch: actual.backend_epoch.clone(),
    })
}

fn backend(error: AgentBackendError) -> RuntimeError {
    RuntimeError::Backend(error.to_string())
}

fn terminal_status(status: pig_protocol::AgentRunStatus) -> Result<RuntimeRunStatus, RuntimeError> {
    Ok(match status {
        pig_protocol::AgentRunStatus::Completed => RuntimeRunStatus::Completed,
        pig_protocol::AgentRunStatus::Cancelled => RuntimeRunStatus::Cancelled,
        pig_protocol::AgentRunStatus::Failed => RuntimeRunStatus::Failed,
        pig_protocol::AgentRunStatus::Lost => RuntimeRunStatus::Lost,
        pig_protocol::AgentRunStatus::Queued | pig_protocol::AgentRunStatus::Running => {
            return Err(RuntimeError::InvalidTransition(
                "backend result is not terminal".to_string(),
            ));
        }
    })
}

impl super::AgentRuntime {
    pub async fn current_execution_identity(
        &self,
        run_id: String,
    ) -> Result<AgentExecutionIdentity, RuntimeError> {
        self.dispatch(move |state| state.execution_identity(&run_id))
            .await
    }

    pub async fn register_execution(
        &self,
        handle: Box<dyn AgentExecutionHandle>,
    ) -> Result<AgentExecutionIdentity, RuntimeError> {
        let executions = self.executions.clone();
        let result = self
            .dispatch(move |state| Ok(executions.register(state, handle)))
            .await?;
        match result {
            Ok(identity) => Ok(identity),
            Err((error, handle)) => {
                drop(handle);
                Err(error)
            }
        }
    }

    async fn take_execution(
        &self,
        run_id: &str,
    ) -> Result<(ExecutionEntry, AgentExecutionIdentity), RuntimeError> {
        let executions = self.executions.clone();
        let run_id = run_id.to_string();
        self.dispatch(move |state| {
            let identity = state.execution_identity(&run_id)?;
            let entry = executions.take(state, &run_id)?;
            Ok((entry, identity))
        })
        .await
    }

    async fn put_execution(&self, run_id: &str, entry: ExecutionEntry) -> Result<(), RuntimeError> {
        let executions = self.executions.clone();
        let run_id = run_id.to_string();
        self.dispatch(move |state| executions.put(state, &run_id, entry))
            .await
    }

    pub async fn send_execution(
        &self,
        run_id: String,
        input: AgentExecutionInput,
    ) -> Result<AgentExecutionReceipt, RuntimeError> {
        let (mut entry, _) = self.take_execution(&run_id).await?;
        match entry.handle.send(input).await {
            Ok(receipt) => {
                self.put_execution(&run_id, entry).await?;
                Ok(receipt)
            }
            Err(error) => {
                self.put_execution(&run_id, entry).await?;
                Err(backend(error))
            }
        }
    }

    pub async fn stop_execution(
        &self,
        run_id: String,
        stop: AgentExecutionStop,
    ) -> Result<AgentExecutionReceipt, RuntimeError> {
        let (mut entry, identity) = self.take_execution(&run_id).await?;
        let reason = match &stop {
            AgentExecutionStop::Graceful { reason } | AgentExecutionStop::Force { reason } => {
                reason.clone()
            }
        };
        match entry.handle.stop(stop).await {
            Ok(receipt) => {
                let executions = self.executions.clone();
                self.dispatch(move |state| {
                    let result = state.request_stop(
                        &run_id,
                        identity.attempt,
                        &identity.backend_epoch,
                        reason,
                    );
                    executions.put(state, &run_id, entry)?;
                    result
                })
                .await?;
                Ok(receipt)
            }
            Err(error) => {
                self.put_execution(&run_id, entry).await?;
                Err(backend(error))
            }
        }
    }

    pub async fn detach_execution(
        &self,
        run_id: String,
    ) -> Result<AgentExecutionReceipt, RuntimeError> {
        let (mut entry, _) = self.take_execution(&run_id).await?;
        match entry.handle.detach().await {
            Ok(receipt) => {
                entry.detached = true;
                let executions = self.executions.clone();
                self.dispatch(move |state| {
                    state.set_connection_state(
                        &run_id,
                        super::RuntimeConnectionState::Disconnected,
                    )?;
                    executions.put(state, &run_id, entry)
                })
                .await?;
                Ok(receipt)
            }
            Err(error) => {
                self.put_execution(&run_id, entry).await?;
                Err(backend(error))
            }
        }
    }

    pub async fn next_execution(
        &self,
        run_id: String,
    ) -> Result<AgentExecutionUpdate, RuntimeError> {
        let (mut entry, identity) = self.take_execution(&run_id).await?;
        match entry.handle.next().await {
            Ok(update) => {
                let committed = update.clone();
                let executions = self.executions.clone();
                self.dispatch(move |state| {
                    match &committed {
                        AgentExecutionUpdate::Settled { result, .. } => {
                            state.settle_run(
                                &run_id,
                                identity.attempt,
                                &identity.backend_epoch,
                                terminal_status(result.status)?,
                                Some(result.stop_reason.clone()),
                                result.usage,
                                result.turns,
                            )?;
                        }
                        AgentExecutionUpdate::Disconnected { .. } => {
                            state.set_connection_state(
                                &run_id,
                                super::RuntimeConnectionState::Disconnected,
                            )?;
                        }
                        AgentExecutionUpdate::Output { .. } => {}
                    }
                    executions.put(state, &run_id, entry)
                })
                .await?;
                Ok(update)
            }
            Err(error) => {
                self.put_execution(&run_id, entry).await?;
                Err(backend(error))
            }
        }
    }

    async fn persisted_release(
        &self,
        run_id: &str,
    ) -> Result<Option<AgentExecutionRelease>, RuntimeError> {
        let snapshot = self.snapshot().await?;
        let run = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .ok_or_else(|| RuntimeError::NotFound(format!("Run {run_id} is not registered")))?;
        let attempt = snapshot
            .attempts
            .iter()
            .find(|attempt| attempt.run_id == run_id)
            .ok_or_else(|| {
                RuntimeError::NotFound(format!("Run {run_id} has no execution attempt"))
            })?;
        let Some(released_at_ms) = attempt.released_at_ms else {
            return Ok(None);
        };
        Ok(Some(AgentExecutionRelease {
            identity: AgentExecutionIdentity {
                run: crate::agent::AgentRunIdentity {
                    session_id: run.session_id.clone(),
                    agent_id: run.agent_id.clone(),
                    run_id: run.run_id.clone(),
                },
                attempt: attempt.attempt,
                backend_epoch: attempt.backend_epoch.clone(),
            },
            cursor: crate::agent::AgentExecutionCursor {
                attempt: attempt.attempt,
                backend_epoch: attempt.backend_epoch.clone(),
                sequence: attempt.last_event_seq,
            },
            released_at_ms,
        }))
    }

    pub async fn release_execution_handle(
        &self,
        run_id: String,
    ) -> Result<AgentExecutionRelease, RuntimeError> {
        let persisted = self.persisted_release(&run_id).await?;
        if persisted.is_some() && !self.executions.has_resources(&run_id) {
            return persisted.ok_or_else(|| {
                RuntimeError::NotFound(format!("Run {run_id} has no release proof"))
            });
        }
        let (mut entry, identity) = self.take_execution(&run_id).await?;
        match entry.handle.release().await {
            Ok(release) => {
                if let Some(persisted) = persisted {
                    self.executions.finish(&run_id);
                    return Ok(persisted);
                }
                let release_run_id = run_id.clone();
                let cursor = release.cursor.sequence;
                let result = self
                    .dispatch(move |state| {
                        state.release_execution_at(
                            &release_run_id,
                            identity.attempt,
                            &identity.backend_epoch,
                            cursor,
                        )
                    })
                    .await;
                match result {
                    Ok(attempt) => {
                        self.executions.finish(&run_id);
                        Ok(AgentExecutionRelease {
                            identity: release.identity,
                            cursor: release.cursor,
                            released_at_ms: attempt
                                .released_at_ms
                                .expect("released attempt timestamp"),
                        })
                    }
                    Err(error) => {
                        self.put_execution(&run_id, entry).await?;
                        Err(error)
                    }
                }
            }
            Err(error) => {
                self.put_execution(&run_id, entry).await?;
                Err(backend(error))
            }
        }
    }

    #[cfg(test)]
    async fn execution_count(&self) -> usize {
        self.executions.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, MutexGuard};

    use futures_util::future::BoxFuture;

    use super::*;
    use crate::agent::runtime::{AgentInitiator, StartRun};
    use crate::agent::{
        AgentBackendFactory, AgentBackendRegistry, AgentExecutionBackend, AgentExecutionCapability,
        AgentExecutionCursor, AgentRunIdentity,
    };
    use crate::store::Store;

    struct ControlOnlyFactory;

    impl AgentBackendFactory for ControlOnlyFactory {
        type Context = ();

        fn capabilities(&self) -> BTreeSet<AgentExecutionCapability> {
            [
                AgentExecutionCapability::Reconnect,
                AgentExecutionCapability::Stop,
                AgentExecutionCapability::Detach,
            ]
            .into_iter()
            .collect()
        }

        fn create(
            &self,
            _context: (),
        ) -> BoxFuture<'static, Result<Box<dyn AgentExecutionBackend>, AgentBackendError>> {
            Box::pin(async {
                Err(AgentBackendError::Failed(
                    "the control test does not execute a backend".into(),
                ))
            })
        }
    }

    #[derive(Default)]
    struct HandleState {
        stopped: bool,
        releases: usize,
        fail_release: bool,
    }

    struct TestHandle {
        identity: AgentExecutionIdentity,
        state: Arc<Mutex<HandleState>>,
        released: Option<AgentExecutionRelease>,
    }

    impl TestHandle {
        fn new(identity: AgentExecutionIdentity, state: Arc<Mutex<HandleState>>) -> Self {
            Self {
                identity,
                state,
                released: None,
            }
        }

        fn lock(&self) -> MutexGuard<'_, HandleState> {
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn receipt(&self) -> AgentExecutionReceipt {
            AgentExecutionReceipt {
                cursor: AgentExecutionCursor {
                    attempt: self.identity.attempt,
                    backend_epoch: self.identity.backend_epoch.clone(),
                    sequence: 0,
                },
            }
        }
    }

    impl AgentExecutionHandle for TestHandle {
        fn identity(&self) -> &AgentExecutionIdentity {
            &self.identity
        }

        fn capabilities(&self) -> BTreeSet<AgentExecutionCapability> {
            [
                AgentExecutionCapability::Reconnect,
                AgentExecutionCapability::Stop,
                AgentExecutionCapability::Detach,
            ]
            .into_iter()
            .collect()
        }

        fn send(
            &mut self,
            _input: AgentExecutionInput,
        ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
            Box::pin(async {
                Err(AgentBackendError::InvalidContext(
                    "send is not used by this test".into(),
                ))
            })
        }

        fn stop(
            &mut self,
            _stop: AgentExecutionStop,
        ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
            Box::pin(async move {
                self.lock().stopped = true;
                Ok(self.receipt())
            })
        }

        fn detach(&mut self) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
            Box::pin(async move { Ok(self.receipt()) })
        }

        fn next(&mut self) -> BoxFuture<'_, Result<AgentExecutionUpdate, AgentBackendError>> {
            Box::pin(async move {
                let stopped = self.lock().stopped;
                let cursor = AgentExecutionCursor {
                    attempt: self.identity.attempt,
                    backend_epoch: self.identity.backend_epoch.clone(),
                    sequence: 1,
                };
                Ok(AgentExecutionUpdate::Settled {
                    cursor,
                    result: crate::agent::AgentBackendResult {
                        status: if stopped {
                            pig_protocol::AgentRunStatus::Cancelled
                        } else {
                            pig_protocol::AgentRunStatus::Completed
                        },
                        stop_reason: if stopped { "test stop" } else { "completed" }.into(),
                        text: String::new(),
                        turns: 1,
                        queued: false,
                        usage: (1, 2, 3),
                    },
                })
            })
        }

        fn release(&mut self) -> BoxFuture<'_, Result<AgentExecutionRelease, AgentBackendError>> {
            Box::pin(async move {
                if let Some(release) = &self.released {
                    return Ok(release.clone());
                }
                {
                    let mut state = self.lock();
                    state.releases += 1;
                    if state.fail_release {
                        return Err(AgentBackendError::Failed(
                            "injected backend release failure".into(),
                        ));
                    }
                }
                let release = AgentExecutionRelease {
                    identity: self.identity.clone(),
                    cursor: AgentExecutionCursor {
                        attempt: self.identity.attempt,
                        backend_epoch: self.identity.backend_epoch.clone(),
                        sequence: 1,
                    },
                    released_at_ms: 0,
                };
                self.released = Some(release.clone());
                Ok(release)
            })
        }
    }

    fn identity(run_id: &str, attempt: u64, epoch: &str) -> AgentExecutionIdentity {
        AgentExecutionIdentity {
            run: AgentRunIdentity {
                session_id: "runtime-execution".into(),
                agent_id: "runtime-execution".into(),
                run_id: run_id.into(),
            },
            attempt,
            backend_epoch: epoch.into(),
        }
    }

    async fn completed_execution(
        test_name: &str,
    ) -> (
        super::super::AgentRuntime,
        Arc<Mutex<Store>>,
        Arc<Mutex<HandleState>>,
        String,
        std::path::PathBuf,
    ) {
        let root = std::env::temp_dir().join(crate::agent::runtime::new_runtime_id(test_name));
        let store = Arc::new(Mutex::new(Store::open(&root).expect("runtime store")));
        let backends = Arc::new(AgentBackendRegistry::default());
        backends
            .register("remote", ControlOnlyFactory)
            .expect("register remote");
        let runtime = super::super::AgentRuntime::spawn_root_with_backends(
            "runtime-execution".into(),
            store.clone(),
            backends,
        )
        .expect("runtime");
        let run_id = "run-release-failure".to_string();
        let (_, attempt) = runtime
            .start_run(StartRun {
                run_id: Some(run_id.clone()),
                agent_id: "runtime-execution".into(),
                backend_id: "remote".into(),
                required_capabilities: [
                    AgentExecutionCapability::Reconnect,
                    AgentExecutionCapability::Stop,
                    AgentExecutionCapability::Detach,
                ]
                .into_iter()
                .collect(),
                initiator: AgentInitiator::System,
                idempotency_key: Some(format!("{test_name}:run")),
                causation_id: None,
                correlation_id: None,
                queued: false,
                watchdog: Default::default(),
            })
            .await
            .expect("start controlled run");
        let state = Arc::new(Mutex::new(HandleState::default()));
        let execution = identity(&run_id, attempt.attempt, &attempt.backend_epoch);
        runtime
            .register_execution(Box::new(TestHandle::new(execution, state.clone())))
            .await
            .expect("register execution");
        let update = runtime
            .next_execution(run_id.clone())
            .await
            .expect("settle execution");
        assert!(matches!(
            update,
            AgentExecutionUpdate::Settled { result, .. }
                if result.status == pig_protocol::AgentRunStatus::Completed
        ));
        (runtime, store, state, run_id, root)
    }

    #[tokio::test]
    async fn backend_release_failure_keeps_the_execution_available_for_retry() {
        let (runtime, _store, state, run_id, root) =
            completed_execution("backend-release-failure").await;
        state.lock().expect("state").fail_release = true;
        let error = runtime
            .release_execution_handle(run_id.clone())
            .await
            .expect_err("inject backend release failure");
        assert!(
            error
                .to_string()
                .contains("injected backend release failure")
        );
        assert_eq!(runtime.execution_count().await, 1);
        assert!(
            runtime.snapshot().await.expect("snapshot").attempts[0]
                .released_at_ms
                .is_none()
        );
        state.lock().expect("state").fail_release = false;
        let released = runtime
            .release_execution_handle(run_id.clone())
            .await
            .expect("retry backend release");
        assert_eq!(state.lock().expect("state").releases, 2);
        let repeated = runtime
            .release_execution_handle(run_id)
            .await
            .expect("idempotent release");
        assert_eq!(repeated, released);
        runtime.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn control_fact_failure_after_release_keeps_an_idempotent_retry_record() {
        let (runtime, _store, state, run_id, root) =
            completed_execution("release-control-fact-failure").await;
        let faults = rusqlite::Connection::open(root.join("store.sqlite")).expect("fault database");
        faults
            .execute(
                "CREATE TRIGGER fail_execution_release
                 BEFORE UPDATE ON runtime_attempts
                 WHEN NEW.released_at_ms IS NOT NULL
                 BEGIN SELECT RAISE(FAIL, 'injected release control fact failure'); END;",
                [],
            )
            .expect("inject control fact failure");
        let error = runtime
            .release_execution_handle(run_id.clone())
            .await
            .expect_err("release fact must fail");
        assert!(
            error
                .to_string()
                .contains("injected release control fact failure")
        );
        assert_eq!(runtime.execution_count().await, 1);
        assert!(
            runtime.snapshot().await.expect("snapshot").attempts[0]
                .released_at_ms
                .is_none()
        );
        faults
            .execute("DROP TRIGGER fail_execution_release", [])
            .expect("remove failure injection");
        let released = runtime
            .release_execution_handle(run_id.clone())
            .await
            .expect("retry control fact commit");
        assert_eq!(state.lock().expect("state").releases, 1);
        assert_eq!(runtime.execution_count().await, 0);
        let repeated = runtime
            .release_execution_handle(run_id)
            .await
            .expect("idempotent committed release");
        assert_eq!(repeated, released);
        runtime.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runtime_fences_attempts_and_releases_handles_before_control_facts() {
        let root = std::env::temp_dir().join(crate::agent::runtime::new_runtime_id(
            "pig-runtime-execution-table",
        ));
        let store = Arc::new(Mutex::new(Store::open(&root).expect("runtime store")));
        let backends = Arc::new(AgentBackendRegistry::default());
        backends
            .register("remote", ControlOnlyFactory)
            .expect("register remote");
        let runtime = super::super::AgentRuntime::spawn_root_with_backends(
            "runtime-execution".into(),
            store,
            backends,
        )
        .expect("runtime");
        let (run, attempt) = runtime
            .start_run(StartRun {
                run_id: Some("run-controlled".into()),
                agent_id: "runtime-execution".into(),
                backend_id: "remote".into(),
                required_capabilities: [
                    AgentExecutionCapability::Reconnect,
                    AgentExecutionCapability::Stop,
                    AgentExecutionCapability::Detach,
                ]
                .into_iter()
                .collect(),
                initiator: AgentInitiator::System,
                idempotency_key: Some("runtime-execution-test".into()),
                causation_id: None,
                correlation_id: None,
                queued: false,
                watchdog: Default::default(),
            })
            .await
            .expect("start controlled run");
        let first_state = Arc::new(Mutex::new(HandleState::default()));
        let first = identity(&run.run_id, attempt.attempt, &attempt.backend_epoch);
        runtime
            .register_execution(Box::new(TestHandle::new(
                first.clone(),
                first_state.clone(),
            )))
            .await
            .expect("register current attempt");
        assert!(matches!(
            runtime
                .register_execution(Box::new(TestHandle::new(
                    first.clone(),
                    first_state.clone()
                )))
                .await,
            Err(RuntimeError::Conflict(_))
        ));
        runtime
            .detach_execution(run.run_id.clone())
            .await
            .expect("detach first attempt");
        let second = runtime
            .reconnect_run_blocking(run.run_id.clone(), 0)
            .expect("second attempt");
        let second_identity = identity(&run.run_id, second.attempt, &second.backend_epoch);
        assert!(matches!(
            runtime
                .register_execution(Box::new(TestHandle::new(first, first_state.clone())))
                .await,
            Err(RuntimeError::StaleAttempt { .. })
        ));
        let second_state = Arc::new(Mutex::new(HandleState::default()));
        runtime
            .register_execution(Box::new(TestHandle::new(
                second_identity.clone(),
                second_state.clone(),
            )))
            .await
            .expect("replace detached attempt");
        runtime
            .stop_execution(
                run.run_id.clone(),
                AgentExecutionStop::Graceful {
                    reason: "test stop".into(),
                },
            )
            .await
            .expect("stop second attempt");
        let update = runtime
            .next_execution(run.run_id.clone())
            .await
            .expect("settle second attempt");
        assert!(matches!(
            update,
            AgentExecutionUpdate::Settled { result, .. }
                if result.status == pig_protocol::AgentRunStatus::Cancelled
        ));
        let mut events = runtime.subscribe();
        let released = runtime
            .release_execution_handle(run.run_id.clone())
            .await
            .expect("release second attempt");
        assert_eq!(released.identity, second_identity);
        while events.recv().await.expect("control event").kind
            != super::super::RuntimeEventKind::ExecutionReleased
        {}
        assert_eq!(second_state.lock().expect("release state").releases, 1);
        assert_eq!(runtime.execution_count().await, 0);
        let repeated = runtime
            .release_execution_handle(run.run_id.clone())
            .await
            .expect("read idempotent release proof");
        assert_eq!(repeated, released);
        let snapshot = runtime.snapshot().await.expect("final snapshot");
        assert_eq!(snapshot.runs[0].status, RuntimeRunStatus::Cancelled);
        assert_eq!(snapshot.runs[0].stop_reason.as_deref(), Some("test stop"));
        assert!(snapshot.attempts[0].released_at_ms.is_some());
        assert_eq!(
            snapshot.attempts[0].last_event_seq,
            released.cursor.sequence
        );
        runtime.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_dir_all(root);
    }
}
