use std::sync::{Arc, Mutex};

use pig_core::agent::runtime::{
    AgentInitiator, AgentRuntime, MailboxMessageKind, RuntimeControlEvent, RuntimeEventKind,
    RuntimeMailboxMessage, RuntimeProjectionDefinition, RuntimeSnapshotView, StartRun,
};
use pig_core::store::Store;

struct ScheduleProjection;

impl RuntimeProjectionDefinition for ScheduleProjection {
    fn key(&self) -> &str {
        "test.schedule"
    }

    fn version(&self) -> u32 {
        1
    }

    fn initial(&self, _snapshot: &RuntimeSnapshotView) -> serde_json::Value {
        serde_json::json!({ "last_schedule": null })
    }

    fn apply(&self, state: &mut serde_json::Value, event: &RuntimeControlEvent) {
        if event.kind != RuntimeEventKind::RunStarted {
            return;
        }
        if event.payload["initiator"]["kind"] == "schedule" {
            state["last_schedule"] = event.payload["initiator"]["schedule_id"].clone();
        }
    }
}

#[tokio::test]
async fn schedule_extension_uses_runtime_without_session_changes() {
    let root = std::env::temp_dir().join(format!(
        "pig-runtime-extension-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = Arc::new(Mutex::new(Store::open(&root).expect("store")));
    let runtime = AgentRuntime::spawn_root("extension-session".into(), store).expect("runtime");
    runtime
        .register_projection_blocking(Box::new(ScheduleProjection))
        .expect("schedule projection");
    let (_run, _attempt) = runtime
        .start_run(StartRun {
            run_id: Some("schedule-run-1".into()),
            agent_id: "extension-session".into(),
            backend_id: "local".into(),
            required_capabilities: Default::default(),
            initiator: AgentInitiator::Schedule {
                schedule_id: "cron-1".into(),
            },
            idempotency_key: Some("cron-1:tick-1".into()),
            causation_id: Some("cron-1".into()),
            correlation_id: Some("cron-1:tick-1".into()),
            queued: false,
            watchdog: Default::default(),
        })
        .await
        .expect("schedule run");
    let snapshot = runtime.projection_snapshot_blocking().expect("projection");
    assert_eq!(
        snapshot.values["test.schedule"].value["last_schedule"],
        "cron-1"
    );
    let agents = runtime.snapshot_blocking().expect("agents").agents;
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].parent_agent_id, None);

    runtime
        .enqueue_mailbox_blocking(RuntimeMailboxMessage::new(
            "extension-session".into(),
            "wake-1".into(),
            "extension-session".into(),
            Some("schedule-run-1".into()),
            MailboxMessageKind::Wake,
            serde_json::json!({ "text": "tick", "notification": null }),
            None,
        ))
        .expect("wake mailbox");
    let claimed = runtime
        .claim_mailbox_blocking("extension-session".into())
        .expect("claim wake");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].kind, MailboxMessageKind::Wake);
    runtime
        .consume_mailbox_blocking("wake-1".into())
        .expect("consume wake");
    runtime.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(root);
}
