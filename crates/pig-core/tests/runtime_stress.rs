use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use pig_core::agent::AgentBackendRegistry;
use pig_core::agent::runtime::{
    AgentInitiator, AgentRuntime, MailboxDeliveryState, MailboxMessageKind, RuntimeAgent,
    RuntimeMailboxDeliveryReceipt, RuntimeMailboxMessage, RuntimeRunStatus, StartRun,
};
use pig_core::store::Store;
use tokio_util::sync::CancellationToken;

fn persist_delivery(path: &std::path::Path, message_id: String, text: String) {
    use std::io::Write as _;

    let record = pig_core::rollout::RolloutRecord::MailboxMessage {
        message_id,
        notification_id: None,
        text,
        sent_at_ms: 0,
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open JSONL");
    writeln!(
        file,
        "{}",
        serde_json::to_string(&record).expect("serialize mailbox record")
    )
    .expect("append JSONL");
    file.sync_data().expect("sync JSONL");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_runtime_operations_remain_bounded_across_restarts() {
    run_mixed_runtime_operations(0).await;
}

async fn run_mixed_runtime_operations(seed: u64) {
    let root = std::env::temp_dir().join(format!(
        "pig-runtime-stress-{seed}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let store = Arc::new(Mutex::new(Store::open(&root).expect("store")));
    let backends = Arc::new(AgentBackendRegistry::with_local());
    let conversation_path = root.join("child.jsonl");
    let mut delivered_ids = BTreeSet::new();

    for round in 0..13_u64 {
        let runtime = AgentRuntime::spawn_root_with_backends(
            "stress-session".into(),
            store.clone(),
            backends.clone(),
        )
        .expect("open runtime");
        if !runtime
            .snapshot_blocking()
            .expect("snapshot")
            .agents
            .iter()
            .any(|agent| agent.agent_id == "child")
        {
            runtime
                .register_agent(RuntimeAgent {
                    session_id: "stress-session".into(),
                    agent_id: "child".into(),
                    parent_agent_id: Some("stress-session".into()),
                    depth: 1,
                    profile: "explore".into(),
                    conversation_path: Some(conversation_path.clone()),
                    created_at_ms: round,
                    closed_at_ms: None,
                })
                .await
                .expect("register child");
        }
        let run_id = format!("run-{round}");
        let request = StartRun {
            run_id: Some(run_id.clone()),
            agent_id: "child".into(),
            backend_id: "local".into(),
            required_capabilities: Default::default(),
            initiator: AgentInitiator::System,
            idempotency_key: Some(format!("spawn-{round}")),
            causation_id: Some(format!("cause-{round}")),
            correlation_id: Some("stress".into()),
            queued: round % 3 == 0,
            watchdog: Default::default(),
        };
        let (run, attempt) = runtime.start_run(request.clone()).await.expect("start run");
        let sequence = runtime
            .snapshot_blocking()
            .expect("snapshot")
            .latest_sequence;
        let (duplicate, duplicate_attempt) = runtime.start_run(request).await.expect("retry start");
        assert_eq!(duplicate.run_id, run.run_id);
        assert_eq!(duplicate_attempt.backend_epoch, attempt.backend_epoch);
        assert_eq!(
            runtime
                .snapshot_blocking()
                .expect("snapshot")
                .latest_sequence,
            sequence
        );
        if run.status == RuntimeRunStatus::Queued {
            runtime
                .mark_run_running_blocking(run_id.clone())
                .expect("start queued run");
        }
        let cancel = CancellationToken::new();
        runtime
            .register_execution_cancel_blocking(run_id.clone(), cancel.clone())
            .expect("register cancellation");

        let message_id = format!("message-{round}");
        let message = RuntimeMailboxMessage::new(
            "stress-session".into(),
            message_id.clone(),
            "child".into(),
            Some(run_id.clone()),
            MailboxMessageKind::Steer,
            serde_json::json!({ "text": format!("input-{round}"), "notification": null }),
            None,
        );
        runtime
            .enqueue_mailbox_blocking(message.clone())
            .expect("enqueue");
        let sequence = runtime
            .snapshot_blocking()
            .expect("snapshot")
            .latest_sequence;
        runtime
            .enqueue_mailbox_blocking(message)
            .expect("retry enqueue");
        assert_eq!(
            runtime
                .snapshot_blocking()
                .expect("snapshot")
                .latest_sequence,
            sequence
        );
        assert_eq!(
            runtime
                .claim_mailbox_blocking("child".into())
                .expect("claim")
                .len(),
            1
        );
        assert!(
            runtime
                .claim_mailbox_blocking("child".into())
                .expect("duplicate claim")
                .is_empty()
        );
        persist_delivery(
            &conversation_path,
            message_id.clone(),
            format!("input-{round}"),
        );
        delivered_ids.insert(message_id);
        let receipt = RuntimeMailboxDeliveryReceipt {
            message_ids: delivered_ids.clone(),
            notification_ids: Default::default(),
        };
        runtime
            .reconcile_mailbox_blocking("child".into(), receipt.clone())
            .expect("acknowledge JSONL");
        let sequence = runtime
            .snapshot_blocking()
            .expect("snapshot")
            .latest_sequence;
        runtime
            .reconcile_mailbox_blocking("child".into(), receipt)
            .expect("retry acknowledgment");
        assert_eq!(
            runtime
                .snapshot_blocking()
                .expect("snapshot")
                .latest_sequence,
            sequence
        );

        let terminal = if round % 2 == 0 {
            runtime
                .request_stop_current_blocking(run_id.clone(), "stress stop".into())
                .expect("stop");
            assert!(cancel.is_cancelled());
            RuntimeRunStatus::Cancelled
        } else {
            assert!(!cancel.is_cancelled());
            RuntimeRunStatus::Completed
        };
        let reason = if terminal == RuntimeRunStatus::Cancelled {
            "stress stop"
        } else {
            "completed"
        };
        runtime
            .settle_current_run_blocking(run_id.clone(), terminal, Some(reason.into()), (0, 0, 0))
            .expect("settle");
        let sequence = runtime
            .snapshot_blocking()
            .expect("snapshot")
            .latest_sequence;
        runtime
            .settle_current_run_blocking(run_id.clone(), terminal, Some(reason.into()), (0, 0, 0))
            .expect("retry settlement");
        let snapshot = runtime.snapshot_blocking().expect("final round snapshot");
        assert_eq!(snapshot.latest_sequence, sequence);
        let released = runtime
            .release_execution_blocking(
                run_id.clone(),
                attempt.attempt,
                attempt.backend_epoch.clone(),
            )
            .expect("release execution");
        assert!(released.released_at_ms.is_some());
        let sequence = runtime
            .snapshot_blocking()
            .expect("snapshot")
            .latest_sequence;
        runtime
            .release_execution_blocking(run_id, attempt.attempt, attempt.backend_epoch)
            .expect("retry release");
        let snapshot = runtime.snapshot_blocking().expect("released snapshot");
        assert_eq!(snapshot.latest_sequence, sequence);
        assert!(snapshot.runs.iter().all(|run| run.status.is_terminal()));
        assert!(
            snapshot
                .mailbox
                .iter()
                .all(|message| message.state == MailboxDeliveryState::Consumed)
        );
        assert!(snapshot.agents.len() <= 2);
        assert!(snapshot.runs.len() <= 4);
        assert!(snapshot.mailbox.len() <= 4);
        let journal_rows: i64 = rusqlite::Connection::open(root.join("store.sqlite"))
            .expect("open journal")
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE session_id = 'stress-session'",
                [],
                |row| row.get(0),
            )
            .expect("journal rows");
        assert!(journal_rows <= 64, "journal grew to {journal_rows} rows");
        runtime.shutdown().await.expect("shutdown");

        if round % 4 == 3 {
            store
                .lock()
                .expect("store lock")
                .request_session_deletion("stress-session")
                .expect("request quiescent deletion");
            store
                .lock()
                .expect("store lock")
                .delete_session_checked("stress-session")
                .expect("delete quiescent session");
            let _ = std::fs::remove_file(&conversation_path);
            delivered_ids.clear();
            let reopened = AgentRuntime::spawn_root_with_backends(
                "stress-session".into(),
                store.clone(),
                backends.clone(),
            )
            .expect("reopen deleted tree");
            let snapshot = reopened.snapshot_blocking().expect("deleted snapshot");
            assert_eq!(snapshot.agents.len(), 1);
            assert!(snapshot.runs.is_empty());
            assert!(snapshot.mailbox.is_empty());
            reopened.shutdown().await.expect("shutdown");
        }
    }

    let runtime = AgentRuntime::spawn_root_with_backends("stress-session".into(), store, backends)
        .expect("final runtime");
    let snapshot = runtime.snapshot_blocking().expect("final snapshot");
    assert_eq!(snapshot.runs.len(), 1);
    assert_eq!(snapshot.mailbox.len(), 1);
    assert!(snapshot.runs.iter().all(|run| run.status.is_terminal()));
    assert!(
        snapshot
            .mailbox
            .iter()
            .all(|message| message.state == MailboxDeliveryState::Consumed)
    );
    runtime.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_runtime_operations_remain_bounded_for_ten_fixed_seeds() {
    for seed in 0..10 {
        run_mixed_runtime_operations(seed).await;
    }
}
