use crate::common::{new_session, setup_with_log};
use pig_protocol::{Event, ExecMode, MessageDelivery, Op};
use pig_provider::mock;
use std::time::{Duration, Instant};

async fn send(agent: &pig_core::AgentHandle, sid: &str, text: &str) {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.to_string(),
            content: text.to_string(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_notification_reaches_model_once_for_every_wire_format() {
    for (name, api_format) in [
        ("chat", "OpenAiChat"),
        ("responses", "OpenAiResponses"),
        ("anthropic", "AnthropicMessages"),
    ] {
        let (config_path, cwd, data_dir, requests) = setup_with_log(&format!(
            "background-shell-notification-{name}-{}",
            std::process::id()
        ));
        let config = std::fs::read_to_string(&config_path)
            .expect("config")
            .replace(
                "api_format = \"OpenAiChat\"",
                &format!("api_format = \"{api_format}\""),
            );
        std::fs::write(&config_path, config).expect("wire format");
        let agent =
            pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
        let sid = new_session(&agent, cwd).await;
        send(&agent, &sid, mock::SCENARIO_BACKGROUND_TRIGGER).await;

        let note = automatic_notification(&agent, "b1", pig_protocol::TaskStatus::Completed).await;
        assert_eq!(note.kind, pig_protocol::TaskKind::Shell);
        assert_eq!(
            note.status,
            pig_protocol::TaskStatus::Completed,
            "background command failed: {note:?}"
        );
        assert_eq!(note.exit_code, Some(0));
        let Some(pig_protocol::TaskNotificationOutput::Preview { text, .. }) = &note.output else {
            panic!("{name}: small command output should be a preview")
        };
        assert!(text.contains(mock::BACKGROUND_TASK_OUTPUT));

        let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).expect("store");
        let (state, channel): (String, String) = db
            .query_row(
                "SELECT delivery_state, delivery_channel FROM task_notifications WHERE notification_id = ?1",
                rusqlite::params![note.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("delivery row");
        assert_eq!(
            (state.as_str(), channel.as_str()),
            ("delivered", "automatic")
        );

        let requests = requests.lock().expect("request log");
        let deliveries = requests
            .iter()
            .filter(|request| request.contains(&note.id))
            .collect::<Vec<_>>();
        assert_eq!(deliveries.len(), 1, "{name}: exactly one model delivery");
        assert!(
            deliveries[0].contains("\"role\":\"user\""),
            "{name}: notification must be user-role"
        );
        assert!(deliveries[0].contains("task.completed"));
        drop(requests);
        agent.shutdown();

        let rollout =
            std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl")))
                .expect("rollout");
        assert_eq!(
            rollout
                .matches("\"type\":\"task_notification_batch\"")
                .count(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_wait_delivery_suppresses_automatic_notification() {
    let (config_path, cwd, data_dir, requests) = setup_with_log("background-task-wait");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    send(&agent, &sid, mock::SCENARIO_TASK_WAIT_TRIGGER).await;

    let mut completions = 0usize;
    let mut automatic_batch = false;
    let deadline = Instant::now() + Duration::from_secs(25);
    while completions < 1 {
        assert!(Instant::now() < deadline, "TaskWait turn did not finish");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), agent.events.recv()).await
        else {
            continue;
        };
        match event {
            Event::TurnComplete { .. } => completions += 1,
            Event::TaskNotificationBatch { .. } => automatic_batch = true,
            _ => {}
        }
    }
    assert!(
        !automatic_batch,
        "TaskWait already delivered the terminal result"
    );
    let notification_id = format!("task:{sid}:b1:completed");
    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).expect("store");
    let (state, channel): (String, String) = db
        .query_row(
            "SELECT delivery_state, delivery_channel FROM task_notifications WHERE notification_id = ?1",
            rusqlite::params![notification_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("delivery row");
    assert_eq!(
        (state.as_str(), channel.as_str()),
        ("delivered", "task_wait")
    );

    send(&agent, &sid, "ECHO_HISTORY after TaskWait").await;
    let deadline = Instant::now() + Duration::from_secs(15);
    while completions < 2 {
        assert!(
            Instant::now() < deadline,
            "the next user turn did not finish"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), agent.events.recv()).await
        else {
            continue;
        };
        match event {
            Event::TurnComplete { .. } => completions += 1,
            Event::TaskNotificationBatch { .. } => automatic_batch = true,
            _ => {}
        }
    }
    assert!(
        !automatic_batch,
        "the next turn must not redeliver the same notification"
    );
    let requests = requests.lock().expect("request log");
    assert!(requests.len() >= 2);
    assert!(
        requests
            .last()
            .expect("next request")
            .contains(&notification_id)
    );
    drop(requests);
    agent.shutdown();

    let rollout = std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl")))
        .expect("rollout");
    assert!(!rollout.contains("\"type\":\"task_notification_batch\""));
    assert!(rollout.contains("\"delivered_notification_ids\":[\"task:"));
}

async fn automatic_notification(
    agent: &pig_core::AgentHandle,
    task_id: &str,
    status: pig_protocol::TaskStatus,
) -> pig_protocol::TaskNotification {
    let mut notification = None;
    crate::common::recv_until(&agent.events, Duration::from_secs(30), |event| {
        match event {
            Event::TaskNotificationBatch { notifications, .. } => {
                if let Some(note) = notifications
                    .iter()
                    .find(|notification| notification.task_id == task_id)
                {
                    assert_eq!(note.status, status);
                    notification = Some(note.clone());
                }
            }
            Event::Error { error, .. } => panic!("background turn failed: {error:?}"),
            _ => {}
        }
        notification.is_some() && matches!(event, Event::TurnComplete { .. })
    })
    .await;
    notification.expect("automatic notification")
}

async fn task_wait_delivery(agent: &pig_core::AgentHandle, notification_id: &str) {
    let mut delivered = false;
    crate::common::recv_until(&agent.events, Duration::from_secs(30), |event| {
        match event {
            Event::ToolCallEnd { output, .. } if output.contains(notification_id) => {
                delivered = true;
            }
            Event::TaskNotificationBatch { notifications, .. }
                if notifications
                    .iter()
                    .any(|notification| notification.id == notification_id) =>
            {
                panic!("TaskWait result was also delivered automatically: {notification_id}")
            }
            Event::Error { error, .. } => panic!("TaskWait turn failed: {error:?}"),
            _ => {}
        }
        delivered && matches!(event, Event::TurnComplete { .. })
    })
    .await;
}

fn assert_delivery(data_dir: &std::path::Path, notification_id: &str, channel: &str) {
    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).expect("store");
    let delivery: (String, String) = db
        .query_row(
            "SELECT delivery_state, delivery_channel FROM task_notifications WHERE notification_id = ?1",
            rusqlite::params![notification_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("delivery row");
    assert_eq!(delivery, ("delivered".to_string(), channel.to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consecutive_background_scenarios_work_for_every_wire_format() {
    for (name, api_format) in [
        ("chat", "OpenAiChat"),
        ("responses", "OpenAiResponses"),
        ("anthropic", "AnthropicMessages"),
    ] {
        let (config_path, cwd, data_dir, requests) = setup_with_log(&format!(
            "background-consecutive-scenarios-{name}-{}",
            std::process::id()
        ));
        let config = std::fs::read_to_string(&config_path)
            .expect("config")
            .replace(
                "api_format = \"OpenAiChat\"",
                &format!("api_format = \"{api_format}\""),
            );
        std::fs::write(&config_path, config).expect("wire format");
        let agent =
            pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
        let sid = new_session(&agent, cwd).await;

        send(&agent, &sid, mock::SCENARIO_BACKGROUND_TRIGGER).await;
        let success =
            automatic_notification(&agent, "b1", pig_protocol::TaskStatus::Completed).await;
        assert_delivery(&data_dir, &success.id, "automatic");
        assert_eq!(
            requests
                .lock()
                .expect("request log")
                .iter()
                .filter(|request| request.contains(&success.id))
                .count(),
            1,
            "{name}: the success notification must reach the model exactly once"
        );

        send(&agent, &sid, mock::SCENARIO_BACKGROUND_FAILURE_TRIGGER).await;
        let failure = automatic_notification(&agent, "b2", pig_protocol::TaskStatus::Failed).await;
        assert_eq!(failure.exit_code, Some(7));
        assert_delivery(&data_dir, &failure.id, "automatic");
        assert_eq!(
            requests
                .lock()
                .expect("request log")
                .iter()
                .filter(|request| request.contains(&failure.id))
                .count(),
            1,
            "{name}: the failure notification must reach the model exactly once"
        );

        let waited_id = format!("task:{sid}:b3:completed");
        send(&agent, &sid, mock::SCENARIO_TASK_WAIT_TRIGGER).await;
        task_wait_delivery(&agent, &waited_id).await;
        assert_delivery(&data_dir, &waited_id, "task_wait");
        let requests = requests.lock().expect("request log");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains(&waited_id))
                .count(),
            1,
            "{name}: the TaskWait result must reach the model exactly once"
        );
        drop(requests);
        agent.shutdown();

        let rollout =
            std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl")))
                .expect("rollout");
        assert_eq!(
            rollout
                .matches("\"type\":\"task_notification_batch\"")
                .count(),
            2,
            "{name}: only the success and failure use automatic batches"
        );
        assert!(
            rollout.contains(&format!("\"delivered_notification_ids\":[\"{waited_id}\"]")),
            "{name}: TaskWait must persist its delivery receipt"
        );
    }
}
