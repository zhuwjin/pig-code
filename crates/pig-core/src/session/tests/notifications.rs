use super::*;

fn notification(task_id: &str) -> pig_protocol::TaskNotification {
    let status = pig_protocol::TaskStatus::Completed;
    pig_protocol::TaskNotification {
        id: crate::task::notification::notification_id("s1", task_id, status),
        task_id: task_id.into(),
        kind: pig_protocol::TaskKind::Shell,
        status,
        exit_code: Some(0),
        severity: pig_protocol::NotificationSeverity::Info,
        description: "cargo test".into(),
        title: "Background shell completed".into(),
        body: "cargo test completed.".into(),
        duration_ms: Some(25),
        turns: None,
        model: None,
        agent_id: None,
        record_path: None,
        output: None,
    }
}

fn mailbox(id: &str) -> Arc<Mutex<VecDeque<String>>> {
    Arc::new(Mutex::new(VecDeque::from([id.to_string()])))
}

fn schedule(fixture: &TrustFixture, note: &pig_protocol::TaskNotification) {
    fixture
        .store()
        .lock()
        .expect("store lock")
        .schedule_notification("s1", note)
        .expect("schedule notification");
}

fn delivery(
    fixture: &TrustFixture,
    id: &str,
) -> (
    pig_protocol::DeliveryState,
    Option<pig_protocol::DeliveryChannel>,
) {
    fixture
        .store()
        .lock()
        .expect("store lock")
        .notification_delivery(id)
        .expect("delivery row")
        .expect("notification exists")
}

fn append_record(fixture: &TrustFixture, record: RolloutRecord) {
    let mut rollout = Rollout::open_append(&fixture.sessions_dir, "s1").expect("open rollout");
    rollout.append(&record).expect("append receipt");
    rollout.sync().expect("sync receipt");
}

fn batch(note: &pig_protocol::TaskNotification) -> RolloutRecord {
    RolloutRecord::TaskNotificationBatch {
        notifications: vec![note.clone()],
        model_text: crate::task::notification::render_notification_batch(std::slice::from_ref(
            note,
        )),
        sent_at_ms: 42,
    }
}

#[test]
fn automatic_delivery_syncs_once_and_survives_reopen() {
    let fixture = TrustFixture::new("notification-once");
    let note = notification("b1");
    let mut session = fixture.create(None);
    schedule(&fixture, &note);
    let (tx, _rx) = async_channel::unbounded();
    let pending = mailbox(&note.id);

    assert_eq!(session.drain_ready_notifications(&pending, &tx), 1);
    assert_eq!(
        delivery(&fixture, &note.id),
        (
            pig_protocol::DeliveryState::Delivered,
            Some(pig_protocol::DeliveryChannel::Automatic)
        )
    );
    assert!(pending.lock().expect("mailbox").is_empty());
    let history_len = session.agent.history.len();
    assert_eq!(session.drain_ready_notifications(&pending, &tx), 0);
    assert_eq!(session.agent.history.len(), history_len);
    drop(session);

    let mut loaded = fixture.load(None);
    assert!(
        fixture
            .store()
            .lock()
            .expect("store lock")
            .pending_notifications("s1")
            .expect("pending")
            .is_empty()
    );
    assert!(
        loaded
            .delivery_receipts
            .lock()
            .expect("receipts")
            .contains_key(&note.id)
    );
    assert_eq!(loaded.drain_ready_notifications(&mailbox(&note.id), &tx), 0);
    let records = Rollout::load(&fixture.sessions_dir.join("s1.jsonl")).expect("records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, RolloutRecord::TaskNotificationBatch { .. }))
            .count(),
        1
    );
}

#[test]
fn rollout_receipt_repairs_missing_sqlite_confirmation() {
    let fixture = TrustFixture::new("notification-reconcile");
    let note = notification("b1");
    let session = fixture.create(None);
    schedule(&fixture, &note);
    drop(session);
    append_record(&fixture, batch(&note));

    let mut loaded = fixture.load(None);
    assert_eq!(
        delivery(&fixture, &note.id),
        (
            pig_protocol::DeliveryState::Delivered,
            Some(pig_protocol::DeliveryChannel::Automatic)
        )
    );
    assert_eq!(
        loaded.drain_ready_notifications(&mailbox(&note.id), &async_channel::unbounded().0),
        0
    );
}

#[test]
fn task_wait_receipt_repairs_missing_sqlite_confirmation() {
    let fixture = TrustFixture::new("wait-reconcile");
    let note = notification("b1");
    let session = fixture.create(None);
    schedule(&fixture, &note);
    drop(session);
    append_record(
        &fixture,
        RolloutRecord::ToolCall {
            tool: "TaskWait".into(),
            summary: "wait".into(),
            arguments: "{\"task_ids\":[\"b1\"]}".into(),
            output: crate::task::notification::render_notification(&note),
            is_error: false,
            stopped: false,
            edit: None,
            agent_card: None,
            agent_cards: vec![],
            images: vec![],
            documents: vec![],
            delivered_notification_ids: vec![note.id.clone()],
        },
    );

    let loaded = fixture.load(None);
    assert_eq!(
        delivery(&fixture, &note.id),
        (
            pig_protocol::DeliveryState::Delivered,
            Some(pig_protocol::DeliveryChannel::TaskWait)
        )
    );
    assert!(
        loaded
            .delivery_receipts
            .lock()
            .expect("receipts")
            .contains_key(&note.id)
    );
}

#[test]
fn scheduled_notification_is_retried_after_a_torn_rollout_tail() {
    let fixture = TrustFixture::new("notification-torn-tail");
    let note = notification("b1");
    let session = fixture.create(None);
    schedule(&fixture, &note);
    drop(session);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.sessions_dir.join("s1.jsonl"))
        .expect("append torn line");
    use std::io::Write as _;
    file.write_all(b"{\"type\":\"task_notification_batch\",\"notifications\":[")
        .expect("write torn line");
    drop(file);

    let mut loaded = fixture.load(None);
    assert_eq!(
        loaded.agent.history.len(),
        1,
        "the torn record is not a delivery receipt"
    );
    let (tx, _rx) = async_channel::unbounded();
    assert_eq!(loaded.drain_ready_notifications(&mailbox(&note.id), &tx), 1);
    assert_eq!(
        delivery(&fixture, &note.id).0,
        pig_protocol::DeliveryState::Delivered
    );
}

#[test]
fn a_failed_rollout_boundary_does_not_acknowledge_delivery() {
    let fixture = TrustFixture::new("notification-write-failure");
    let note = notification("b1");
    let mut session = fixture.create(None);
    schedule(&fixture, &note);
    session.rollout_failure = Some(CoreError::Internal {
        detail: "injected rollout failure".into(),
    });
    let (tx, _rx) = async_channel::unbounded();
    let mailbox = mailbox(&note.id);

    assert_eq!(session.drain_ready_notifications(&mailbox, &tx), 0);
    assert!(session.agent.history.is_empty());
    assert_eq!(
        delivery(&fixture, &note.id).0,
        pig_protocol::DeliveryState::Scheduled
    );
    let db = rusqlite::Connection::open(fixture.data_dir.join("store.sqlite")).expect("store");
    let last_error: Option<String> = db
        .query_row(
            "SELECT last_error FROM task_notifications WHERE notification_id = ?1",
            rusqlite::params![note.id],
            |row| row.get(0),
        )
        .expect("error detail");
    assert!(
        last_error
            .as_deref()
            .is_some_and(|error| error.contains("injected rollout failure"))
    );
    drop(session);

    let mut loaded = fixture.load(None);
    assert_eq!(loaded.drain_ready_notifications(&mailbox, &tx), 1);
}

#[test]
fn interrupted_tasks_become_lost_once_and_keep_their_ids() {
    let fixture = TrustFixture::new("notification-lost");
    let session = fixture.create(None);
    let store = fixture.store();
    store
        .lock()
        .expect("store lock")
        .upsert_background_task(
            "s1",
            &crate::store::BackgroundTask {
                summary: pig_protocol::TaskSummary {
                    id: "b9".into(),
                    kind: pig_protocol::TaskKind::Shell,
                    command: "cargo test".into(),
                    status: pig_protocol::TaskStatus::Running,
                    exit_code: None,
                    reason: None,
                    started_at_ms: 1,
                    ended_at_ms: None,
                    output_tail: "partial".into(),
                    agent_id: None,
                    spill_path: None,
                },
                detached: true,
                record_path: None,
                result_path: None,
                turns: None,
                model: None,
                run: None,
                terminal_notification_suppressed: false,
            },
        )
        .expect("running task");
    drop(session);

    let loaded = fixture.load(None);
    let pending = fixture
        .store()
        .lock()
        .expect("store lock")
        .pending_notifications("s1")
        .expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].status, pig_protocol::TaskStatus::Lost);
    assert!(pending[0].body.contains("Do not assume"));
    assert_eq!(
        crate::task::snapshot(&loaded.state.tasks)[0].status,
        pig_protocol::TaskStatus::Lost
    );
    assert_eq!(
        crate::task::snapshot(&loaded.state.tasks)[0].output_tail,
        "partial"
    );
    drop(loaded);

    let loaded = fixture.load(None);
    assert_eq!(
        fixture
            .store()
            .lock()
            .expect("store lock")
            .pending_notifications("s1")
            .expect("pending")
            .len(),
        1,
        "reopening must not create a second Lost notification"
    );
    let new_id = crate::task::register_agent_task(
        &loaded.state,
        "new run".into(),
        tokio_util::sync::CancellationToken::new(),
        "a-new".into(),
    );
    assert_eq!(new_id, "b10", "task IDs are never reused after recovery");
}

#[test]
fn terminal_fact_without_outbox_is_recovered_once() {
    let fixture = TrustFixture::new("notification-terminal-recovery");
    let session = fixture.create(None);
    fixture
        .store()
        .lock()
        .expect("store lock")
        .upsert_background_task(
            "s1",
            &crate::store::BackgroundTask {
                summary: pig_protocol::TaskSummary {
                    id: "b4".into(),
                    kind: pig_protocol::TaskKind::Shell,
                    command: "cargo test".into(),
                    status: pig_protocol::TaskStatus::Completed,
                    exit_code: Some(0),
                    reason: None,
                    started_at_ms: 1,
                    ended_at_ms: Some(2),
                    output_tail: "partial".into(),
                    agent_id: None,
                    spill_path: None,
                },
                detached: true,
                record_path: None,
                result_path: None,
                turns: Some(4),
                model: Some("mock".into()),
                run: None,
                terminal_notification_suppressed: false,
            },
        )
        .expect("terminal fact");
    drop(session);

    let loaded = fixture.load(None);
    let pending = fixture
        .store()
        .lock()
        .expect("store lock")
        .pending_notifications("s1")
        .expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].status, pig_protocol::TaskStatus::Completed);
    assert_eq!(pending[0].turns, Some(4));
    assert_eq!(pending[0].model.as_deref(), Some("mock"));
    assert!(matches!(
        pending[0].output,
        Some(pig_protocol::TaskNotificationOutput::Preview { ref text, .. }) if text == "partial"
    ));
    assert_eq!(
        crate::task::snapshot(&loaded.state.tasks)[0].status,
        pig_protocol::TaskStatus::Completed
    );
    drop(loaded);

    let _ = fixture.load(None);
    assert_eq!(
        fixture
            .store()
            .lock()
            .expect("store lock")
            .pending_notifications("s1")
            .expect("pending")
            .len(),
        1,
        "reopening cannot schedule the same terminal fact twice"
    );
}

#[test]
fn recall_requeues_only_the_revoked_delivery() {
    let fixture = TrustFixture::new("notification-recall");
    let note = notification("b1");
    let mut session = fixture.create(None);
    session.agent.history = vec![ChatMsg::system("sys".into()), ChatMsg::user("hi".into())];
    session.user_ixs = vec![Some(1)];
    session.record(&user());
    session.sync_rollout();
    schedule(&fixture, &note);
    let (tx, _rx) = async_channel::unbounded();
    assert_eq!(
        session.drain_ready_notifications(&mailbox(&note.id), &tx),
        1
    );
    assert_eq!(
        delivery(&fixture, &note.id).0,
        pig_protocol::DeliveryState::Delivered
    );

    session.recall_last_message(&tx).expect("recall");
    assert_eq!(
        delivery(&fixture, &note.id).0,
        pig_protocol::DeliveryState::Scheduled
    );
    assert!(
        !session
            .delivery_receipts
            .lock()
            .expect("receipts")
            .contains_key(&note.id)
    );
    assert!(!session.notification_reconcile_required);
    let mut loaded = fixture.load(None);
    assert_eq!(loaded.drain_ready_notifications(&mailbox(&note.id), &tx), 1);
}

#[tokio::test]
async fn compaction_preserves_delivery_and_reminds_about_active_tasks() {
    let fixture = TrustFixture::new("notification-compaction");
    let note = notification("b1");
    let mut session = fixture.create(None);
    session.agent.history = vec![
        ChatMsg::system("sys".into()),
        ChatMsg::user("old".into()),
        ChatMsg::assistant("answer".into(), vec![], None),
        ChatMsg::user("latest".into()),
    ];
    session.user_ixs = vec![Some(1), Some(3)];
    session.record(&user());
    session.record(&user());
    session.sync_rollout();
    schedule(&fixture, &note);
    let (tx, _rx) = async_channel::unbounded();
    assert_eq!(
        session.drain_ready_notifications(&mailbox(&note.id), &tx),
        1
    );
    let active_id = crate::task::register_agent_task(
        &session.state,
        "still running".into(),
        tokio_util::sync::CancellationToken::new(),
        "a-active".into(),
    );

    let outcome = session
        .run_compact(
            None,
            CompactTrigger::Watermark,
            None,
            false,
            None,
            &tx,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
    assert_eq!(outcome, CompactOutcome::Completed);
    let reminders = session
        .agent
        .history
        .iter()
        .filter(|message| {
            message
                .content
                .as_deref()
                .is_some_and(|text| text.contains("<background-tasks>"))
        })
        .count();
    assert_eq!(reminders, 1);
    assert!(
        session.agent.history[1]
            .content
            .as_deref()
            .is_some_and(|text| text.contains(&active_id))
    );
    assert_eq!(
        session.drain_ready_notifications(&mailbox(&note.id), &tx),
        0
    );
    drop(session);

    let mut loaded = fixture.load(None);
    assert!(
        fixture
            .store()
            .lock()
            .expect("store lock")
            .pending_notifications("s1")
            .expect("pending")
            .is_empty()
    );
    assert_eq!(loaded.drain_ready_notifications(&mailbox(&note.id), &tx), 0);
    assert!(loaded.agent.history.iter().any(|message| {
        message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("<background-tasks>") && text.contains(&active_id))
    }));
}
