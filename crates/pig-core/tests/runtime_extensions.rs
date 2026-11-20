use std::sync::{Arc, Mutex};

use pig_core::agent::runtime::{
    AgentInitiator, AgentRuntime, MailboxMessageKind, RuntimeAgent, RuntimeControlEvent,
    RuntimeEventKind, RuntimeMailboxMessage, RuntimeProjectionDefinition, RuntimeSnapshotView,
    StartRun,
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
    let runtime =
        AgentRuntime::spawn_root("extension-session".into(), store.clone()).expect("runtime");
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
            MailboxMessageKind::Custom("test.domain_wake".into()),
            serde_json::json!({ "text": "tick", "notification": null }),
            None,
        ))
        .expect("wake mailbox");
    let claimed = runtime
        .claim_mailbox_blocking("extension-session".into())
        .expect("claim wake");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].kind.as_str(), "test.domain_wake");
    runtime
        .consume_mailbox_blocking("wake-1".into())
        .expect("consume wake");
    runtime.shutdown().await.expect("shutdown");
    let restarted = AgentRuntime::spawn_root("extension-session".into(), store)
        .expect("restart with the custom mailbox kind");
    let restored = restarted.snapshot_blocking().expect("restored mailbox");
    let message = restored
        .mailbox
        .iter()
        .find(|message| message.message_id == "wake-1")
        .expect("custom message survives restart");
    assert_eq!(message.kind.as_str(), "test.domain_wake");
    assert_eq!(
        message.state,
        pig_core::agent::runtime::MailboxDeliveryState::Consumed
    );
    restarted.shutdown().await.expect("shutdown restarted");
    let _ = std::fs::remove_dir_all(root);
}

struct CatalogProjection;

impl RuntimeProjectionDefinition for CatalogProjection {
    fn key(&self) -> &str {
        "test.catalog"
    }

    fn version(&self) -> u32 {
        4
    }

    fn initial(&self, snapshot: &RuntimeSnapshotView) -> serde_json::Value {
        serde_json::json!({
            "agents": snapshot.agents.len(),
            "runs": snapshot.runs.len(),
            "initialized_at": snapshot.latest_sequence,
        })
    }

    fn apply(&self, state: &mut serde_json::Value, event: &RuntimeControlEvent) {
        if event.kind == RuntimeEventKind::AgentRegistered {
            state["agents"] = serde_json::json!(state["agents"].as_u64().unwrap_or(0) + 1);
        }
    }
}

#[tokio::test]
async fn catalog_projection_registers_and_resyncs_without_core_changes() {
    let root = std::env::temp_dir().join(format!(
        "pig-catalog-projection-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = Arc::new(Mutex::new(Store::open(&root).expect("store")));
    let runtime = AgentRuntime::spawn_root("catalog-session".into(), store).expect("runtime");
    runtime
        .register_projection_blocking(Box::new(CatalogProjection))
        .expect("register catalog projection");
    runtime
        .register_agent(RuntimeAgent {
            session_id: "catalog-session".into(),
            agent_id: "catalog-child".into(),
            parent_agent_id: Some("catalog-session".into()),
            depth: 1,
            profile: "explore".into(),
            conversation_path: None,
            created_at_ms: 1,
            closed_at_ms: None,
        })
        .await
        .expect("register catalog child");
    let snapshot = runtime
        .projection_snapshot_blocking()
        .expect("catalog snapshot");
    assert_eq!(snapshot.values["test.catalog"].version, 4);
    assert_eq!(snapshot.values["test.catalog"].value["agents"], 2);
    runtime
        .resync_projection_blocking("test.catalog".into())
        .expect("resync catalog projection");
    let restored = runtime
        .projection_snapshot_blocking()
        .expect("restored catalog");
    assert_eq!(restored.values["test.catalog"].value["agents"], 2);
    assert_eq!(
        restored.consistent_sequence,
        runtime
            .snapshot_blocking()
            .expect("runtime snapshot")
            .latest_sequence
    );
    runtime.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(root);
}
