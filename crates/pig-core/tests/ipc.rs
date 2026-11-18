use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pig_core::agent::ipc::{
    IpcAgentBackendFactory, IpcCapability, IpcEventBatch, IpcRunResult, IpcRunSpec, ipc_events,
    ipc_reconnect, ipc_release, ipc_start, ipc_start_with_requirements, ipc_stop,
};
use pig_core::agent::runtime::{
    AgentInitiator, AgentRuntime, RuntimeConnectionState, RuntimeError, RuntimeRunStatus, StartRun,
};
use pig_core::agent::{
    AgentEventCursor, AgentExecutionActivation, AgentExecutionIdentity, AgentExecutionRequest,
    AgentExecutionStop, AgentExecutionUpdate, AgentRunIdentity,
};
use pig_core::store::Store;
use pig_provider::mock;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn start_worker() -> (SocketAddr, Child) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pig-agent-worker"));
    command
        .arg("--bind=127.0.0.1:0")
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
    let (address, mut worker) = start_worker().await;
    let store = Arc::new(Mutex::new(
        Store::open(&root.join("parent-store")).expect("parent store"),
    ));

    let parent = runtime(store.clone(), address);
    let unsupported = ipc_start_with_requirements(
        address,
        "ipc-unsupported",
        IpcRunSpec {
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
        &completed_run.run_id,
        IpcRunSpec {
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
    ipc_release(address, &completed_run.run_id, false)
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
        &active_run.run_id,
        IpcRunSpec {
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

    ipc_stop(address, &active_run.run_id)
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
    ipc_release(address, &active_run.run_id, false)
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
    let (address, mut worker) = start_worker().await;
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
    let result = timeout(
        Duration::from_secs(30),
        parent.backends().execute(
            "ipc",
            IpcRunSpec {
                config_path: Some(config_path),
                cwd,
                data_dir: root.join("worker-registry"),
                prompt: "read the mock file and summarize".to_string(),
            },
            execution,
        ),
    )
    .await
    .expect("registry IPC execution timeout")
    .expect("registry IPC execution");
    assert_eq!(result.status, pig_protocol::AgentRunStatus::Completed);
    assert!(result.text.contains(mock::MOCK_REPLY_MARKER));
    parent
        .settle_run(
            run.run_id.clone(),
            attempt.attempt,
            attempt.backend_epoch.clone(),
            RuntimeRunStatus::Completed,
            Some(result.stop_reason),
            result.usage,
        )
        .await
        .expect("settle registered run");
    ipc_release(address, &run.run_id, false)
        .await
        .expect("first release");
    ipc_release(address, &run.run_id, false)
        .await
        .expect("idempotent release");
    assert!(
        parent
            .release_execution_blocking(run.run_id, attempt.attempt, attempt.backend_epoch,)
            .expect("runtime release")
            .released_at_ms
            .is_some()
    );
    parent.shutdown().await.expect("shutdown");
    worker.kill().await.expect("stop worker");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn execution_handle_reconnects_stops_and_releases_real_worker() {
    let (config_path, cwd, root) = crate::common::setup("ipc-execution-handle");
    let (address, mut worker) = start_worker().await;
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
