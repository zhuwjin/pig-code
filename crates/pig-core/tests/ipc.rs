use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pig_core::agent::ipc::{
    IPC_PROTOCOL_VERSION, IpcAgentBackendFactory, IpcAgentExecutionHandle, IpcCapability,
    IpcEventBatch, IpcRunResult, IpcRunSpec, ipc_events, ipc_reconnect, ipc_release, ipc_start,
    ipc_start_with_requirements, ipc_stop,
};
use pig_core::agent::runtime::{
    AgentInitiator, AgentRuntime, RuntimeConnectionState, RuntimeError, RuntimeRunStatus, StartRun,
};
use pig_core::agent::{
    AgentEventCursor, AgentExecutionActivation, AgentExecutionCursor, AgentExecutionHandle,
    AgentExecutionIdentity, AgentExecutionInput, AgentExecutionRequest, AgentExecutionStop,
    AgentExecutionUpdate, AgentRunIdentity,
};
use pig_core::store::Store;
use pig_provider::mock;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn start_worker(state_dir: &std::path::Path) -> (SocketAddr, Child) {
    start_worker_at(state_dir, "127.0.0.1:0".parse().expect("loopback address")).await
}

async fn start_worker_at(state_dir: &std::path::Path, bind: SocketAddr) -> (SocketAddr, Child) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pig-agent-worker"));
    command
        .arg(format!("--bind={bind}"))
        .arg(format!("--state-dir={}", state_dir.display()))
        .env("PIG_SELFTEST", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn Pig Agent worker");
    let stdout = child.stdout.take().expect("worker stdout");
    let line = timeout(
        Duration::from_secs(10),
        BufReader::new(stdout).lines().next_line(),
    )
    .await
    .expect("worker ready timeout")
    .expect("worker ready IO")
    .expect("worker ready line");
    let address = line
        .strip_prefix("PIG_AGENT_WORKER_ADDR=")
        .expect("worker address line")
        .parse()
        .expect("worker socket address");
    (address, child)
}

fn runtime(store: Arc<Mutex<Store>>, address: SocketAddr) -> AgentRuntime {
    let backends = std::sync::Arc::new(pig_core::agent::AgentBackendRegistry::with_local());
    backends
        .register("ipc", IpcAgentBackendFactory::new(address))
        .expect("register IPC backend");
    AgentRuntime::spawn_root_with_backends("ipc-parent".to_string(), store, backends)
        .expect("parent runtime")
}

fn start_run(run_id: &str) -> StartRun {
    StartRun {
        run_id: Some(run_id.to_string()),
        agent_id: "ipc-parent".to_string(),
        backend_id: "ipc".to_string(),
        required_capabilities: [
            pig_core::agent::AgentExecutionCapability::Reconnect,
            pig_core::agent::AgentExecutionCapability::Stop,
        ]
        .into_iter()
        .collect(),
        initiator: AgentInitiator::System,
        idempotency_key: Some(format!("test:{run_id}")),
        causation_id: Some("ipc-test".to_string()),
        correlation_id: Some("ipc-test".to_string()),
        queued: false,
        watchdog: Default::default(),
    }
}

async fn next_batch(address: SocketAddr, run_id: &str, cursor: &mut u64) -> IpcEventBatch {
    let batch = ipc_events(address, run_id, *cursor)
        .await
        .expect("IPC events");
    *cursor = batch.latest_sequence;
    batch
}

async fn wait_terminal(address: SocketAddr, run_id: &str, cursor: &mut u64) -> IpcRunResult {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let batch = next_batch(address, run_id, cursor).await;
        if let Some(terminal) = batch.terminal {
            return terminal;
        }
        assert!(Instant::now() < deadline, "IPC run did not settle");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_worker_completes_and_reconnects_after_parent_restart() {
    let (config_path, cwd, root) = crate::common::setup("ipc-real-worker");
    let (address, mut worker) = start_worker(&root.join("worker-state")).await;
    let store = Arc::new(Mutex::new(
        Store::open(&root.join("parent-store")).expect("parent store"),
    ));

    let parent = runtime(store.clone(), address);
    let unsupported = ipc_start_with_requirements(
        address,
        worker_identity("ipc-unsupported", 1),
        IpcRunSpec {
            resume_from_run_id: None,
            config_path: Some(config_path.clone()),
            cwd: cwd.clone(),
            data_dir: root.join("worker-unsupported"),
            prompt: "must not start".to_string(),
        },
        [IpcCapability::StructuredOutput].into_iter().collect(),
    )
    .await;
    assert!(
        unsupported
            .expect_err("unsupported capability")
            .contains("StructuredOutput")
    );
    assert!(ipc_events(address, "ipc-unsupported", 0).await.is_err());

    let (completed_run, _) = parent
        .start_run(start_run("ipc-completed"))
        .await
        .expect("completed run registration");
    ipc_start(
        address,
        worker_identity(&completed_run.run_id, 1),
        IpcRunSpec {
            resume_from_run_id: None,
            config_path: Some(config_path.clone()),
            cwd: cwd.clone(),
            data_dir: root.join("worker-completed"),
            prompt: "read the mock file and summarize".to_string(),
        },
    )
    .await
    .expect("start completed IPC run");
    let mut completed_cursor = 0;
    let terminal = wait_terminal(address, &completed_run.run_id, &mut completed_cursor).await;
    assert_eq!(terminal.status, "completed");
    assert!(
        terminal.text.contains(mock::MOCK_REPLY_MARKER),
        "{}",
        terminal.text
    );
    assert!(completed_cursor > 0);
    ipc_release(address, &worker_identity(&completed_run.run_id, 1), false)
        .await
        .expect("release completed run");
    parent
        .settle_run(
            completed_run.run_id.clone(),
            1,
            parent
                .snapshot()
                .await
                .expect("snapshot")
                .attempts
                .into_iter()
                .find(|attempt| attempt.run_id == completed_run.run_id)
                .expect("attempt")
                .backend_epoch,
            RuntimeRunStatus::Completed,
            Some(terminal.stop_reason),
            terminal.usage,
        )
        .await
        .expect("settle completed run");
    let completed_attempt = parent
        .snapshot()
        .await
        .expect("snapshot")
        .attempts
        .into_iter()
        .find(|attempt| attempt.run_id == completed_run.run_id)
        .expect("completed attempt");
    let released = parent
        .release_execution_blocking(
            completed_run.run_id.clone(),
            completed_attempt.attempt,
            completed_attempt.backend_epoch,
        )
        .expect("release runtime execution");
    assert!(released.released_at_ms.is_some());
    assert_eq!(
        parent
            .release_execution_blocking(
                completed_run.run_id,
                released.attempt,
                released.backend_epoch,
            )
            .expect("idempotent runtime release")
            .released_at_ms,
        released.released_at_ms
    );

    let (active_run, first_attempt) = parent
        .start_run(start_run("ipc-reconnect"))
        .await
        .expect("active run registration");
    ipc_start(
        address,
        worker_identity(&active_run.run_id, 1),
        IpcRunSpec {
            resume_from_run_id: None,
            config_path: Some(config_path),
            cwd,
            data_dir: root.join("worker-reconnect"),
            prompt: mock::SCENARIO_SLOW_TRIGGER.to_string(),
        },
    )
    .await
    .expect("start active IPC run");
    let mut active_cursor = 0;
    let active_deadline = Instant::now() + Duration::from_secs(10);
    while active_cursor == 0 {
        next_batch(address, &active_run.run_id, &mut active_cursor).await;
        assert!(
            Instant::now() < active_deadline,
            "IPC run produced no events"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    parent
        .disconnect_run_blocking(active_run.run_id.clone())
        .expect("disconnect");
    parent.shutdown().await.expect("parent shutdown");

    let reconnected = runtime(store, address);
    let before = reconnected.snapshot().await.expect("reconnected snapshot");
    assert_eq!(
        before
            .runs
            .iter()
            .find(|run| run.run_id == active_run.run_id)
            .expect("active run")
            .status,
        RuntimeRunStatus::Running
    );
    assert_eq!(
        before
            .attempts
            .iter()
            .find(|attempt| attempt.run_id == active_run.run_id)
            .expect("disconnected attempt")
            .connection_state,
        RuntimeConnectionState::Disconnected
    );
    let worker_info = ipc_reconnect(address, &active_run.run_id)
        .await
        .expect("worker reconnect");
    let second_attempt = reconnected
        .reconnect_run_blocking(active_run.run_id.clone(), worker_info.latest_sequence)
        .expect("runtime reconnect");
    assert_eq!(second_attempt.attempt, first_attempt.attempt + 1);
    assert!(matches!(
        reconnected
            .settle_run(
                active_run.run_id.clone(),
                first_attempt.attempt,
                first_attempt.backend_epoch,
                RuntimeRunStatus::Cancelled,
                Some("stale".to_string()),
                (0, 0, 0),
            )
            .await,
        Err(RuntimeError::StaleAttempt { .. })
    ));

    ipc_stop(address, &worker_identity(&active_run.run_id, 1))
        .await
        .expect("stop reconnected run");
    active_cursor = worker_info.latest_sequence;
    let stopped = wait_terminal(address, &active_run.run_id, &mut active_cursor).await;
    assert_eq!(stopped.status, "cancelled");
    reconnected
        .settle_run(
            active_run.run_id.clone(),
            second_attempt.attempt,
            second_attempt.backend_epoch,
            RuntimeRunStatus::Cancelled,
            Some(stopped.stop_reason),
            stopped.usage,
        )
        .await
        .expect("settle stopped run");
    let stopped_attempt = reconnected
        .snapshot()
        .await
        .expect("stopped snapshot")
        .attempts
        .into_iter()
        .find(|attempt| attempt.run_id == active_run.run_id)
        .expect("stopped attempt");
    assert!(
        reconnected
            .release_execution_blocking(
                active_run.run_id.clone(),
                stopped_attempt.attempt,
                stopped_attempt.backend_epoch,
            )
            .expect("release stopped execution")
            .released_at_ms
            .is_some()
    );
    ipc_release(address, &worker_identity(&active_run.run_id, 1), false)
        .await
        .expect("release stopped run");
    assert!(ipc_events(address, &active_run.run_id, 0).await.is_err());
    reconnected.shutdown().await.expect("shutdown");
    worker.kill().await.expect("stop worker");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backend_registry_executes_real_ipc_and_releases_idempotently() {
    let (config_path, cwd, root) = crate::common::setup("ipc-backend-registry");
    let (address, mut worker) = start_worker(&root.join("worker-state")).await;
    let store = Arc::new(Mutex::new(
        Store::open(&root.join("parent-store")).expect("parent store"),
    ));
    let parent = runtime(store, address);

    let mut unsupported = start_run("ipc-registry-unsupported");
    unsupported
        .required_capabilities
        .insert(pig_core::agent::AgentExecutionCapability::StructuredOutput);
    assert!(matches!(
        parent.start_run(unsupported).await,
        Err(RuntimeError::Backend(_))
    ));
    assert!(
        parent
            .snapshot()
            .await
            .expect("empty snapshot")
            .runs
            .is_empty()
    );

    let request = start_run("ipc-registry-completed");
    let required_capabilities = request.required_capabilities.clone();
    let (run, attempt) = parent.start_run(request).await.expect("register run");
    let (events, _event_receiver) = async_channel::unbounded();
    let mut execution = pig_core::agent::AgentExecutionRequest::new(
        pig_core::agent::AgentRunIdentity {
            session_id: "ipc-parent".into(),
            agent_id: "ipc-parent".into(),
            run_id: run.run_id.clone(),
        },
        events,
        tokio_util::sync::CancellationToken::new(),
        pig_core::agent::AgentExecutionActivation::Start,
    );
    execution.required_capabilities = required_capabilities;
    execution = execution.with_execution_identity(AgentExecutionIdentity {
        run: AgentRunIdentity {
            session_id: "ipc-parent".into(),
            agent_id: "ipc-parent".into(),
            run_id: run.run_id.clone(),
        },
        attempt: attempt.attempt,
        backend_epoch: attempt.backend_epoch.clone(),
    });
    let handle = parent
        .backends()
        .start(
            "ipc",
            IpcRunSpec {
                resume_from_run_id: None,
                config_path: Some(config_path),
                cwd,
                data_dir: root.join("worker-registry"),
                prompt: "read the mock file and summarize".to_string(),
            },
            execution,
        )
        .await
        .expect("start registry IPC handle");
    parent
        .register_execution(handle)
        .await
        .expect("register handle in the parent runtime");
    let result = timeout(Duration::from_secs(30), async {
        loop {
            match parent.next_execution(run.run_id.clone()).await? {
                AgentExecutionUpdate::Output { .. } => {}
                AgentExecutionUpdate::Settled { result, .. } => break Ok(result),
                AgentExecutionUpdate::Disconnected { detail, .. } => {
                    return Err(RuntimeError::Backend(detail));
                }
            }
        }
    })
    .await
    .expect("registry IPC execution timeout")
    .expect("registry IPC execution");
    assert_eq!(result.status, pig_protocol::AgentRunStatus::Completed);
    assert!(result.text.contains(mock::MOCK_REPLY_MARKER));
    let first = parent
        .release_execution_handle(run.run_id.clone())
        .await
        .expect("first release");
    let repeated = parent
        .release_execution_handle(run.run_id.clone())
        .await
        .expect("idempotent release");
    assert_eq!(repeated, first);
    let snapshot = parent.snapshot().await.expect("runtime snapshot");
    assert_eq!(snapshot.runs[0].status, RuntimeRunStatus::Completed);
    assert_eq!(snapshot.attempts[0].last_event_seq, first.cursor.sequence);
    assert!(snapshot.attempts[0].released_at_ms.is_some());
    parent.shutdown().await.expect("shutdown");
    worker.kill().await.expect("stop worker");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn execution_handle_reconnects_stops_and_releases_real_worker() {
    let (config_path, cwd, root) = crate::common::setup("ipc-execution-handle");
    let (address, mut worker) = start_worker(&root.join("worker-state")).await;
    let run_id = "ipc-handle-run";
    let identity = |attempt: u64| AgentExecutionIdentity {
        run: AgentRunIdentity {
            session_id: "ipc-handle-parent".into(),
            agent_id: "ipc-handle-agent".into(),
            run_id: run_id.into(),
        },
        attempt,
        backend_epoch: format!("ipc-handle-epoch-{attempt}"),
    };
    let backends = pig_core::agent::AgentBackendRegistry::with_local();
    backends
        .register("ipc", IpcAgentBackendFactory::new(address))
        .expect("register IPC backend");
    let spec = IpcRunSpec {
        resume_from_run_id: None,
        config_path: Some(config_path),
        cwd,
        data_dir: root.join("worker-handle"),
        prompt: mock::SCENARIO_SLOW_TRIGGER.to_string(),
    };
    let execution = identity(1);
    let (events, _receiver) = async_channel::unbounded();
    let mut request = AgentExecutionRequest::new(
        execution.run.clone(),
        events,
        CancellationToken::new(),
        AgentExecutionActivation::Start,
    )
    .with_execution_identity(execution);
    request.required_capabilities = [
        pig_core::agent::AgentExecutionCapability::Steer,
        pig_core::agent::AgentExecutionCapability::Reconnect,
        pig_core::agent::AgentExecutionCapability::Stop,
    ]
    .into_iter()
    .collect();
    let mut handle = backends
        .start("ipc", spec.clone(), request)
        .await
        .expect("start IPC handle through registry");
    timeout(Duration::from_secs(10), async {
        loop {
            match handle.next().await.expect("first handle update") {
                AgentExecutionUpdate::Output { .. } => break,
                AgentExecutionUpdate::Disconnected { detail, .. } => {
                    panic!("unexpected disconnect: {detail}")
                }
                AgentExecutionUpdate::Settled { .. } => {
                    panic!("IPC handle settled before detach")
                }
            }
        }
    })
    .await
    .expect("first handle output");
    let detached = handle.detach().await.expect("detach active handle");
    let remote = ipc_reconnect(address, run_id)
        .await
        .expect("remote remains registered");
    assert!(remote.terminal.is_none());

    let execution = identity(2);
    let (events, _receiver) = async_channel::unbounded();
    let mut request = AgentExecutionRequest::new(
        execution.run.clone(),
        events,
        CancellationToken::new(),
        AgentExecutionActivation::Reconnect,
    )
    .with_execution_identity(execution);
    request.cursor = Some(AgentEventCursor {
        run: identity(1).run,
        sequence: detached.cursor.sequence,
    });
    request.required_capabilities = [
        pig_core::agent::AgentExecutionCapability::Steer,
        pig_core::agent::AgentExecutionCapability::Reconnect,
        pig_core::agent::AgentExecutionCapability::Stop,
    ]
    .into_iter()
    .collect();
    let mut reconnected = backends
        .start("ipc", spec, request)
        .await
        .expect("reconnect through registry with a new attempt");
    assert_eq!(reconnected.identity().attempt, 2);
    assert!(matches!(
        reconnected
            .stop(AgentExecutionStop::Force {
                reason: "unsupported force".into(),
            })
            .await,
        Err(pig_core::agent::AgentBackendError::UnsupportedCapability {
            capability: pig_core::agent::AgentExecutionCapability::ForceStop,
            ..
        })
    ));
    reconnected
        .stop(AgentExecutionStop::Graceful {
            reason: "test stop".into(),
        })
        .await
        .expect("graceful remote stop");
    let (terminal_cursor, result) = timeout(Duration::from_secs(30), async {
        loop {
            match reconnected.next().await.expect("reconnected update") {
                AgentExecutionUpdate::Output { .. } => {}
                AgentExecutionUpdate::Disconnected { detail, .. } => {
                    panic!("unexpected reconnect loss: {detail}")
                }
                AgentExecutionUpdate::Settled { cursor, result } => {
                    break (cursor, result);
                }
            }
        }
    })
    .await
    .expect("remote stop settlement");
    assert_eq!(result.status, pig_protocol::AgentRunStatus::Cancelled);
    let released = reconnected.release().await.expect("release remote worker");
    assert_eq!(released.cursor, terminal_cursor);
    assert_eq!(released.identity, identity(2));
    let repeated = reconnected.release().await.expect("idempotent release");
    assert_eq!(repeated, released);
    assert!(ipc_events(address, run_id, 0).await.is_err());
    worker.kill().await.expect("stop worker");
    let _ = std::fs::remove_dir_all(root);
}

async fn collect_handle_events(
    handle: &mut IpcAgentExecutionHandle,
) -> (Vec<u64>, pig_core::agent::AgentBackendResult) {
    timeout(Duration::from_secs(30), async {
        let mut sequences = Vec::new();
        loop {
            match handle.next().await.expect("worker handle update") {
                AgentExecutionUpdate::Output { sequence, .. } => sequences.push(sequence),
                AgentExecutionUpdate::Settled { result, .. } => return (sequences, result),
                AgentExecutionUpdate::Disconnected { detail, .. } => {
                    panic!("unexpected worker disconnect: {detail}")
                }
            }
        }
    })
    .await
    .expect("worker handle settlement")
}

fn worker_identity(run_id: &str, attempt: u64) -> AgentExecutionIdentity {
    AgentExecutionIdentity {
        run: AgentRunIdentity {
            session_id: "worker-restart-parent".into(),
            agent_id: "worker-restart-agent".into(),
            run_id: run_id.into(),
        },
        attempt,
        backend_epoch: format!("worker-restart-epoch-{attempt}"),
    }
}

fn worker_address() -> SocketAddr {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve worker port");
    let address = reservation.local_addr().expect("worker address");
    drop(reservation);
    address
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_restart_replays_a_completed_run_without_duplicates() {
    let (config_path, cwd, root) = crate::common::setup("ipc-worker-completed-restart");
    let address = worker_address();
    let state = root.join("worker-state");
    let run_id = "ipc-worker-completed";
    let (actual_address, mut worker) = start_worker_at(&state, address).await;
    assert_eq!(actual_address, address);
    let mut handle = IpcAgentExecutionHandle::start(
        address,
        worker_identity(run_id, 1),
        IpcRunSpec {
            resume_from_run_id: None,
            config_path: Some(config_path),
            cwd,
            data_dir: root.join("worker-data"),
            prompt: "read the mock file and summarize".to_string(),
        },
    )
    .await
    .expect("start completed worker run");
    let (original_events, original_result) = collect_handle_events(&mut handle).await;
    assert_eq!(
        original_result.status,
        pig_protocol::AgentRunStatus::Completed
    );
    worker.kill().await.expect("kill original worker");
    drop(handle);

    let (restarted_address, mut restarted) = start_worker_at(&state, address).await;
    assert_eq!(restarted_address, address);
    let mut recovered = IpcAgentExecutionHandle::reconnect(
        address,
        worker_identity(run_id, 2),
        pig_core::agent::AgentExecutionCursor {
            attempt: 1,
            backend_epoch: "worker-restart-epoch-1".into(),
            sequence: 0,
        },
    )
    .await
    .expect("recover completed run after worker restart");
    let (replayed_events, recovered_result) = collect_handle_events(&mut recovered).await;
    assert!(replayed_events.len() >= original_events.len());
    assert_eq!(&replayed_events[..original_events.len()], original_events);
    assert_eq!(
        replayed_events,
        (1..=replayed_events.len() as u64).collect::<Vec<_>>()
    );
    assert_eq!(recovered_result, original_result);
    recovered.release().await.expect("release recovered run");
    assert!(ipc_events(address, run_id, 0).await.is_err());
    restarted.kill().await.expect("stop restarted worker");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_restart_recovers_an_unsettled_run_as_failed_and_releasable() {
    let (config_path, cwd, root) = crate::common::setup("ipc-worker-active-restart");
    let address = worker_address();
    let state = root.join("worker-state");
    let run_id = "ipc-worker-active";
    let (_, mut worker) = start_worker_at(&state, address).await;
    let mut handle = IpcAgentExecutionHandle::start(
        address,
        worker_identity(run_id, 1),
        IpcRunSpec {
            resume_from_run_id: None,
            config_path: Some(config_path),
            cwd,
            data_dir: root.join("worker-data"),
            prompt: mock::SCENARIO_SLOW_TRIGGER.to_string(),
        },
    )
    .await
    .expect("start active worker run");
    timeout(Duration::from_secs(10), async {
        loop {
            match handle.next().await.expect("active worker update") {
                AgentExecutionUpdate::Output { .. } => break,
                AgentExecutionUpdate::Settled { .. } => panic!("run settled before restart"),
                AgentExecutionUpdate::Disconnected { detail, .. } => panic!("{detail}"),
            }
        }
    })
    .await
    .expect("active run output");
    worker.kill().await.expect("kill active worker");
    drop(handle);

    let (_, mut restarted) = start_worker_at(&state, address).await;
    let mut recovered = IpcAgentExecutionHandle::reconnect(
        address,
        worker_identity(run_id, 2),
        pig_core::agent::AgentExecutionCursor {
            attempt: 1,
            backend_epoch: "worker-restart-epoch-1".into(),
            sequence: 0,
        },
    )
    .await
    .expect("recover interrupted run");
    assert!(matches!(
        recovered
            .send(AgentExecutionInput {
                message_id: "input-after-worker-restart".into(),
                payload: serde_json::json!({"text": "must not be queued"}),
            })
            .await,
        Err(pig_core::agent::AgentBackendError::Disconnected(_))
    ));
    let (events, result) = collect_handle_events(&mut recovered).await;
    assert!(!events.is_empty());
    assert_eq!(result.status, pig_protocol::AgentRunStatus::Failed);
    assert!(result.stop_reason.contains("worker restarted"));
    recovered.release().await.expect("release interrupted run");
    restarted.kill().await.expect("stop restarted worker");
    let _ = std::fs::remove_dir_all(root);
}

struct TcpProxy {
    address: SocketAddr,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl TcpProxy {
    async fn start(target: SocketAddr, bind: SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .expect("bind TCP proxy");
        let address = listener.local_addr().expect("proxy address");
        let shutdown = CancellationToken::new();
        let cancelled = shutdown.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = cancelled.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((client, _)) = accepted else { break };
                let connection_cancelled = cancelled.clone();
                tokio::spawn(async move {
                    let Ok(mut worker) = tokio::net::TcpStream::connect(target).await else {
                        return;
                    };
                    let mut client = client;
                    tokio::select! {
                        () = connection_cancelled.cancelled() => {}
                        _ = tokio::io::copy_bidirectional(&mut client, &mut worker) => {}
                    }
                });
            }
        });
        Self {
            address,
            shutdown,
            task,
        }
    }

    async fn stop(self) {
        self.shutdown.cancel();
        let _ = timeout(Duration::from_secs(5), self.task).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn execution_handle_steers_and_reconnects_after_a_real_transport_disconnect() {
    if !crate::common::native_sandbox_usable() {
        crate::common::assert_native_sandbox_rejection().await;
        return;
    }
    let (config_path, cwd, root) = crate::common::setup("ipc-transport-disconnect");
    let (worker_address, mut worker) = start_worker(&root.join("worker-state")).await;
    let proxy_address = self::worker_address();
    let proxy = TcpProxy::start(worker_address, proxy_address).await;
    assert_eq!(proxy.address, proxy_address);
    let run_id = "ipc-network-reconnect";
    let mut handle = IpcAgentExecutionHandle::start(
        proxy_address,
        worker_identity(run_id, 1),
        IpcRunSpec {
            resume_from_run_id: None,
            config_path: Some(config_path),
            cwd,
            data_dir: root.join("worker-data"),
            prompt: mock::SCENARIO_SLOW_TRIGGER.to_string(),
        },
    )
    .await
    .expect("start through TCP proxy");
    timeout(Duration::from_secs(10), async {
        loop {
            match handle.next().await.expect("proxied execution update") {
                AgentExecutionUpdate::Output {
                    event: pig_protocol::Event::ToolCallBegin { .. },
                    ..
                } => break,
                AgentExecutionUpdate::Output { .. } => {}
                AgentExecutionUpdate::Settled { .. } => panic!("run settled before disconnect"),
                AgentExecutionUpdate::Disconnected { detail, .. } => panic!("{detail}"),
            }
        }
    })
    .await
    .expect("tool boundary before disconnect");
    proxy.stop().await;
    let cursor = timeout(Duration::from_secs(5), async {
        loop {
            match handle.next().await.expect("transport failure update") {
                AgentExecutionUpdate::Disconnected { cursor, .. } => break cursor,
                AgentExecutionUpdate::Output { .. } => {}
                AgentExecutionUpdate::Settled { .. } => panic!("run settled before disconnect"),
            }
        }
    })
    .await
    .expect("handle observed real network failure");
    drop(handle);
    assert!(ipc_reconnect(proxy_address, run_id).await.is_err());
    assert!(
        ipc_reconnect(worker_address, run_id)
            .await
            .expect("worker survived proxy disconnect")
            .terminal
            .is_none()
    );

    let proxy = TcpProxy::start(worker_address, proxy_address).await;
    let mut reconnected = IpcAgentExecutionHandle::reconnect(
        proxy_address,
        worker_identity(run_id, 2),
        AgentExecutionCursor {
            attempt: 1,
            backend_epoch: "worker-restart-epoch-1".into(),
            sequence: cursor.sequence,
        },
    )
    .await
    .expect("reconnect after transport recovery");
    let input = AgentExecutionInput {
        message_id: "ipc-steer-1".into(),
        payload: serde_json::json!({
            "text": "IPC steer input",
            "delivery": "steer",
        }),
    };
    let receipt = reconnected
        .send(input.clone())
        .await
        .expect("steer through production handle");
    assert_eq!(
        reconnected
            .send(input)
            .await
            .expect("duplicate input receipt"),
        receipt
    );
    let mut sequences = Vec::new();
    let result = timeout(Duration::from_secs(30), async {
        loop {
            match reconnected
                .next()
                .await
                .expect("reconnected execution update")
            {
                AgentExecutionUpdate::Output { sequence, event } => {
                    assert!(sequence > cursor.sequence);
                    assert!(!sequences.contains(&sequence));
                    sequences.push(sequence);
                    if matches!(
                        event,
                        pig_protocol::Event::SteerChanged { ref items, .. }
                            if items.iter().any(|item| item.id == "ipc-steer-1" && item.text == "IPC steer input")
                    ) {
                        return true;
                    }
                }
                AgentExecutionUpdate::Settled { .. } => return false,
                AgentExecutionUpdate::Disconnected { detail, .. } => panic!("{detail}"),
            }
        }
    })
    .await
    .expect("steer event after reconnect");
    assert!(result, "IPC send did not enter the steer mailbox");
    reconnected
        .stop(AgentExecutionStop::Graceful {
            reason: "transport test complete".into(),
        })
        .await
        .expect("stop the still-running worker");
    let steered_sequence_count = sequences.len();
    let (tail, result) = collect_handle_events(&mut reconnected).await;
    assert_eq!(result.status, pig_protocol::AgentRunStatus::Cancelled);
    assert!(tail.iter().all(|sequence| *sequence > cursor.sequence));
    let total = steered_sequence_count + tail.len();
    sequences.extend(tail);
    sequences.sort_unstable();
    sequences.dedup();
    assert_eq!(sequences.len(), total);
    reconnected
        .release()
        .await
        .expect("release reconnected run");
    proxy.stop().await;
    worker.kill().await.expect("stop worker");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn execution_handle_deduplicates_consumed_input_and_waits_for_queued_turn() {
    let (config_path, cwd, root) = crate::common::setup("ipc-consumed-input");
    let (address, mut worker) = start_worker(&root.join("worker-state")).await;
    let run_id = "ipc-consumed-input";
    let mut handle = IpcAgentExecutionHandle::start(
        address,
        worker_identity(run_id, 1),
        IpcRunSpec {
            resume_from_run_id: None,
            config_path: Some(config_path),
            cwd,
            data_dir: root.join("worker-data"),
            prompt: "read the mock file and summarize".to_string(),
        },
    )
    .await
    .expect("start worker run");
    let input = AgentExecutionInput {
        message_id: "ipc-queued-input-1".into(),
        payload: serde_json::json!({
            "text": "IPC queued input",
            "delivery": "queue",
        }),
    };
    timeout(Duration::from_secs(10), async {
        loop {
            match handle.next().await.expect("initial worker update") {
                AgentExecutionUpdate::Output {
                    event: pig_protocol::Event::ToolCallBegin { .. },
                    ..
                } => break,
                AgentExecutionUpdate::Output { .. } => {}
                AgentExecutionUpdate::Settled { .. } => panic!("settled before queue input"),
                AgentExecutionUpdate::Disconnected { detail, .. } => panic!("{detail}"),
            }
        }
    })
    .await
    .expect("initial tool call");
    handle
        .send(input.clone())
        .await
        .expect("queue input through production handle");

    let mut completions = 0;
    let mut visible_inputs = 0;
    let result = timeout(Duration::from_secs(30), async {
        loop {
            match handle.next().await.expect("queued worker update") {
                AgentExecutionUpdate::Output { event, .. } => {
                    if matches!(event, pig_protocol::Event::TurnComplete { .. }) {
                        completions += 1;
                    }
                    if matches!(
                        event,
                        pig_protocol::Event::UserMessage { ref text, .. }
                            if text == "IPC queued input"
                    ) {
                        visible_inputs += 1;
                        handle
                            .send(input.clone())
                            .await
                            .expect("consumed duplicate receipt");
                        handle
                            .send(input.clone())
                            .await
                            .expect("repeated duplicate receipt");
                    }
                }
                AgentExecutionUpdate::Settled { result, .. } => break result,
                AgentExecutionUpdate::Disconnected { detail, .. } => panic!("{detail}"),
            }
        }
    })
    .await
    .expect("queued turn settlement");
    assert_eq!(result.status, pig_protocol::AgentRunStatus::Completed);
    assert_eq!(
        completions, 2,
        "the queued turn must finish before settlement"
    );
    assert_eq!(
        visible_inputs, 1,
        "consumed input must reach the model once"
    );
    handle.release().await.expect("release queued run");
    worker.kill().await.expect("stop worker");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_execution_handle_cannot_control_or_release_a_reconnected_run() {
    if !crate::common::native_sandbox_usable() {
        crate::common::assert_native_sandbox_rejection().await;
        return;
    }
    let (config_path, cwd, root) = crate::common::setup("ipc-stale-execution");
    let (address, mut worker) = start_worker(&root.join("worker-state")).await;
    let run_id = "ipc-stale-execution";
    let mut first = IpcAgentExecutionHandle::start(
        address,
        worker_identity(run_id, 1),
        IpcRunSpec {
            resume_from_run_id: None,
            config_path: Some(config_path),
            cwd,
            data_dir: root.join("worker-data"),
            prompt: mock::SCENARIO_SLOW_TRIGGER.to_string(),
        },
    )
    .await
    .expect("start first attempt");
    timeout(Duration::from_secs(10), async {
        loop {
            match first.next().await.expect("first attempt update") {
                AgentExecutionUpdate::Output {
                    event: pig_protocol::Event::ToolCallBegin { .. },
                    ..
                } => break,
                AgentExecutionUpdate::Output { .. } => {}
                AgentExecutionUpdate::Settled { .. } => panic!("settled before reconnect"),
                AgentExecutionUpdate::Disconnected { detail, .. } => panic!("{detail}"),
            }
        }
    })
    .await
    .expect("first attempt is running");
    let mut second = IpcAgentExecutionHandle::reconnect(
        address,
        worker_identity(run_id, 2),
        AgentExecutionCursor {
            attempt: 1,
            backend_epoch: "worker-restart-epoch-1".into(),
            sequence: 0,
        },
    )
    .await
    .expect("second attempt owns the remote execution");

    let stale_send = first
        .send(AgentExecutionInput {
            message_id: "stale-input".into(),
            payload: serde_json::json!({"text": "must not be accepted"}),
        })
        .await
        .expect_err("stale input must be rejected");
    assert!(stale_send.to_string().contains("stale"));
    let stale_stop = first
        .stop(AgentExecutionStop::Graceful {
            reason: "stale stop".into(),
        })
        .await
        .expect_err("stale stop must be rejected");
    assert!(stale_stop.to_string().contains("stale"));
    let stale_release = first
        .release()
        .await
        .expect_err("stale release must be rejected");
    assert!(stale_release.to_string().contains("stale"));
    assert!(
        ipc_reconnect(address, run_id)
            .await
            .expect("remote remains active")
            .terminal
            .is_none()
    );
    second
        .stop(AgentExecutionStop::Graceful {
            reason: "current attempt stop".into(),
        })
        .await
        .expect("current attempt can stop");
    let (_, result) = collect_handle_events(&mut second).await;
    assert_eq!(result.status, pig_protocol::AgentRunStatus::Cancelled);
    let (_, first_result) = collect_handle_events(&mut first).await;
    assert_eq!(first_result.status, pig_protocol::AgentRunStatus::Cancelled);
    worker.kill().await.expect("restart after reconnect");
    let (_, mut restarted) = start_worker_at(&root.join("worker-state"), address).await;
    assert!(
        first
            .release()
            .await
            .expect_err("release must restore and authorize the recorded owner")
            .to_string()
            .contains("stale")
    );
    let mut third = IpcAgentExecutionHandle::reconnect(
        address,
        worker_identity(run_id, 3),
        AgentExecutionCursor {
            attempt: 2,
            backend_epoch: "worker-restart-epoch-2".into(),
            sequence: 0,
        },
    )
    .await
    .expect("persisted attempt survives worker restart");
    let (_, recovered) = collect_handle_events(&mut third).await;
    assert_eq!(recovered.status, pig_protocol::AgentRunStatus::Cancelled);
    assert!(
        second
            .release()
            .await
            .expect_err("previous owner must become stale")
            .to_string()
            .contains("stale")
    );
    third.release().await.expect("latest attempt release");
    restarted.kill().await.expect("stop restarted worker");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_send_response_is_reconciled_without_replaying_the_input() {
    let (config_path, cwd, root) = crate::common::setup("ipc-lost-send-response");
    let (address, mut worker) = start_worker(&root.join("worker-state")).await;
    let run_id = "ipc-lost-send-response";
    let first_identity = worker_identity(run_id, 1);
    let mut first = IpcAgentExecutionHandle::start(
        address,
        first_identity.clone(),
        IpcRunSpec {
            resume_from_run_id: None,
            config_path: Some(config_path),
            cwd,
            data_dir: root.join("worker-data"),
            prompt: "read the mock file and summarize".to_string(),
        },
    )
    .await
    .expect("start worker run");
    timeout(Duration::from_secs(10), async {
        loop {
            match first.next().await.expect("initial worker update") {
                AgentExecutionUpdate::Output {
                    event: pig_protocol::Event::ToolCallBegin { .. },
                    ..
                } => break,
                AgentExecutionUpdate::Output { .. } => {}
                AgentExecutionUpdate::Settled { .. } => panic!("settled before lost response"),
                AgentExecutionUpdate::Disconnected { detail, .. } => panic!("{detail}"),
            }
        }
    })
    .await
    .expect("initial tool call");

    let input = AgentExecutionInput {
        message_id: "ipc-lost-response-input-1".into(),
        payload: serde_json::json!({
            "text": "IPC response loss input",
            "delivery": "queue",
        }),
    };
    let mut connection = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect for response loss");
    let request = serde_json::json!({
        "id": "lost-send-response",
        "protocol": IPC_PROTOCOL_VERSION,
        "method": "send",
        "params": {
            "run_id": run_id,
            "execution": first_identity,
            "input": input.clone(),
        },
    });
    let mut encoded = serde_json::to_vec(&request).expect("encode send request");
    encoded.push(b'\n');
    connection.write_all(&encoded).await.expect("write request");
    connection
        .shutdown()
        .await
        .expect("close request write side");
    drop(connection);

    timeout(Duration::from_secs(10), async {
        loop {
            let batch = ipc_events(address, run_id, 0)
                .await
                .expect("inspect mailbox");
            if batch.events.iter().any(|record| {
                matches!(
                    &record.event,
                    pig_protocol::Event::QueueChanged { items, .. }
                        if items.iter().any(|item| item.id == input.message_id)
                )
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("request executed although its response was lost");
    drop(first);
    let mut recovered = IpcAgentExecutionHandle::reconnect(
        address,
        worker_identity(run_id, 2),
        AgentExecutionCursor {
            attempt: 1,
            backend_epoch: "worker-restart-epoch-1".into(),
            sequence: 0,
        },
    )
    .await
    .expect("reconnect after response loss");
    recovered
        .send(input.clone())
        .await
        .expect("retry unknown send outcome");

    let mut visible_inputs = 0;
    let mut completions = 0;
    let (_, result) = timeout(Duration::from_secs(30), async {
        let mut sequences = Vec::new();
        loop {
            match recovered.next().await.expect("reconciled worker update") {
                AgentExecutionUpdate::Output { sequence, event } => {
                    assert!(!sequences.contains(&sequence));
                    sequences.push(sequence);
                    if matches!(
                        event,
                        pig_protocol::Event::UserMessage { ref text, .. }
                            if text == "IPC response loss input"
                    ) {
                        visible_inputs += 1;
                    }
                    if matches!(event, pig_protocol::Event::TurnComplete { .. }) {
                        completions += 1;
                    }
                }
                AgentExecutionUpdate::Settled { result, .. } => break (sequences, result),
                AgentExecutionUpdate::Disconnected { detail, .. } => panic!("{detail}"),
            }
        }
    })
    .await
    .expect("settle after unknown send outcome");
    assert_eq!(visible_inputs, 1);
    assert_eq!(completions, 2);
    assert_eq!(result.status, pig_protocol::AgentRunStatus::Completed);
    recovered.release().await.expect("release reconciled run");
    worker.kill().await.expect("stop worker");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_crash_resumes_a_durable_pending_input_in_a_new_run() {
    if !crate::common::native_sandbox_usable() {
        crate::common::assert_native_sandbox_rejection().await;
        return;
    }
    let (config_path, cwd, root) = crate::common::setup("ipc-crash-pending-input");
    let state = root.join("worker-state");
    let data_dir = root.join("worker-data");
    let address = worker_address();
    let (_, mut worker) = start_worker_at(&state, address).await;
    let old_run_id = "ipc-crash-pending-old";
    let old_identity = worker_identity(old_run_id, 1);
    let mut old = IpcAgentExecutionHandle::start(
        address,
        old_identity.clone(),
        IpcRunSpec {
            resume_from_run_id: None,
            config_path: Some(config_path.clone()),
            cwd: cwd.clone(),
            data_dir: data_dir.clone(),
            prompt: mock::SCENARIO_SLOW_TRIGGER.to_string(),
        },
    )
    .await
    .expect("start run before crash");
    timeout(Duration::from_secs(10), async {
        loop {
            match old.next().await.expect("pre-crash update") {
                AgentExecutionUpdate::Output {
                    event: pig_protocol::Event::ToolCallBegin { .. },
                    ..
                } => break,
                AgentExecutionUpdate::Output { .. } => {}
                AgentExecutionUpdate::Settled { .. } => panic!("settled before crash"),
                AgentExecutionUpdate::Disconnected { detail, .. } => panic!("{detail}"),
            }
        }
    })
    .await
    .expect("active tool before crash");
    old.send(AgentExecutionInput {
        message_id: "ipc-crash-pending-input-1".into(),
        payload: serde_json::json!({
            "text": "IPC crash pending input",
            "delivery": "queue",
        }),
    })
    .await
    .expect("queue durable input");
    timeout(Duration::from_secs(10), async {
        loop {
            let batch = ipc_events(address, old_run_id, 0)
                .await
                .expect("pending event");
            if batch.events.iter().any(|record| {
                matches!(
                    &record.event,
                    pig_protocol::Event::QueueChanged { items, .. }
                        if items.iter().any(|item| item.id == "ipc-crash-pending-input-1")
                )
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("durable pending admission before crash");
    old.stop(AgentExecutionStop::Graceful {
        reason: "pause pending input before worker crash".into(),
    })
    .await
    .expect("stop with pending input");
    let (_, stopped) = collect_handle_events(&mut old).await;
    assert_eq!(stopped.status, pig_protocol::AgentRunStatus::Cancelled);
    worker
        .kill()
        .await
        .expect("crash worker with pending input");
    drop(old);
    let (_, mut restarted) = start_worker_at(&state, address).await;
    let previous = ipc_reconnect(address, old_run_id)
        .await
        .expect("restore old run")
        .terminal
        .expect("stopped old run remains settled");
    assert_eq!(previous.status, "cancelled");

    let new_run_id = "ipc-crash-pending-new";
    let mut resumed = IpcAgentExecutionHandle::start(
        address,
        worker_identity(new_run_id, 1),
        IpcRunSpec {
            resume_from_run_id: Some(old_run_id.into()),
            config_path: Some(config_path),
            cwd,
            data_dir,
            prompt: "IPC resume follow-up".to_string(),
        },
    )
    .await
    .expect("resume the durable session in a new run");
    let mut pending_visible = 0;
    let mut completions = 0;
    let mut completion_identities = Vec::new();
    let mut turn_inputs = Vec::new();
    let mut active_run = None;
    let result = timeout(Duration::from_secs(30), async {
        loop {
            match resumed.next().await.expect("resumed worker update") {
                AgentExecutionUpdate::Output { event, .. } => {
                    if let pig_protocol::Event::TurnStarted { run, .. } = &event {
                        active_run = run.as_ref().map(|run| run.run_id.clone());
                    }
                    if let pig_protocol::Event::UserMessage { text, .. } = &event {
                        turn_inputs.push((text.clone(), active_run.clone()));
                    }
                    if matches!(
                        event,
                        pig_protocol::Event::UserMessage { ref text, .. }
                            if text == "IPC crash pending input"
                    ) {
                        pending_visible += 1;
                    }
                    if let pig_protocol::Event::TurnComplete { run, .. } = &event {
                        completions += 1;
                        completion_identities.push(run.clone());
                    }
                }
                AgentExecutionUpdate::Settled { result, .. } => break result,
                AgentExecutionUpdate::Disconnected { detail, .. } => panic!("{detail}"),
            }
        }
    })
    .await
    .expect("resume pending input");
    assert_eq!(pending_visible, 1);
    assert_eq!(
        completions, 2,
        "completion identities: {completion_identities:#?}; inputs: {turn_inputs:#?}"
    );
    assert_eq!(result.status, pig_protocol::AgentRunStatus::Completed);
    resumed.release().await.expect("release resumed run");
    ipc_release(address, &old_identity, false)
        .await
        .expect("release old run record");
    restarted.kill().await.expect("stop restarted worker");
    let _ = std::fs::remove_dir_all(root);
}
