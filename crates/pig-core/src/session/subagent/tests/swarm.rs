use super::*;

/// Full foreground swarm path: 10 items (> the 8-slot cap) through the
/// mock provider; every child gets a task entry and settles Completed, and
/// all JSONL writer claims are released before run_swarm returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_swarm_above_slot_cap_completes_and_releases_children() {
    let _test_lock = crate::task::SUBAGENT_SLOT_TEST_LOCK.lock().await;
    let started = Instant::now();
    let (mut session, parent, root) = swarm_test_session("swarm-complete");
    let items: Vec<String> = (0..(crate::task::MAX_CONCURRENT_SUBAGENTS + 2))
        .map(|index| format!("item-{index}"))
        .collect();
    let call = ToolCall {
        id: "call_foreground_swarm".into(),
        name: "AgentSwarm".into(),
        arguments: serde_json::json!({
            "prompt_template": "SUBAGENT_CHILD:GREP {{item}}",
            "items": items,
        })
        .to_string(),
    };
    let (tx, _rx) = async_channel::unbounded();
    let cancel = CancellationToken::new();
    let outcome = session
        .run_swarm(&call, "parent-swarm", &parent, &tx, &cancel)
        .await;
    let SubagentOutcome::Finished {
        note,
        is_error,
        cards,
        ..
    } = outcome
    else {
        panic!("foreground swarm should finish");
    };
    assert!(!is_error, "{note}");
    assert_eq!(cards.len(), 10, "one card per child");
    assert!(
        note.contains("10 subagents total (10 completed / 0 failed / 0 cancelled)"),
        "{note}"
    );
    let tasks = session.state.tasks.lock().expect("task registry lock");
    assert_eq!(tasks.len(), 10);
    for task in tasks.iter() {
        assert!(
            matches!(task.status, pig_protocol::TaskStatus::Completed),
            "{} settled as {:?}",
            task.id,
            task.status
        );
    }
    drop(tasks);
    for card in &cards {
        assert!(
            !session.state.agent_writer_active(&card.agent_id),
            "{} still owns its context writer",
            card.agent_id
        );
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "foreground swarm took too long"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// The same >slot-count foreground swarm with the parent already
/// cancelled: every queued/running child settles Killed and no writer or
/// registry entry is left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_swarm_cancel_settles_every_child() {
    let _test_lock = crate::task::SUBAGENT_SLOT_TEST_LOCK.lock().await;
    let (mut session, parent, root) = swarm_test_session("swarm-cancel");
    let items: Vec<String> = (0..(crate::task::MAX_CONCURRENT_SUBAGENTS + 2))
        .map(|index| format!("item-{index}"))
        .collect();
    let call = ToolCall {
        id: "call_cancelled_swarm".into(),
        name: "AgentSwarm".into(),
        arguments: serde_json::json!({
            "prompt_template": "SUBAGENT_CHILD:GREP {{item}}",
            "items": items,
        })
        .to_string(),
    };
    let (tx, _rx) = async_channel::unbounded();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let outcome = session
        .run_swarm(&call, "parent-swarm", &parent, &tx, &cancel)
        .await;
    assert!(
        matches!(outcome, SubagentOutcome::Cancelled { .. }),
        "parent cancellation wins over a partial aggregate"
    );
    let tasks = session.state.tasks.lock().expect("task registry lock");
    assert_eq!(tasks.len(), 10);
    for task in tasks.iter() {
        assert!(
            matches!(task.status, pig_protocol::TaskStatus::Killed),
            "{} settled as {:?}",
            task.id,
            task.status
        );
        assert!(task.ended_at.is_some());
    }
    drop(tasks);
    assert!(
        session
            .state
            .agent_writers
            .lock()
            .expect("writers")
            .is_empty(),
        "no child may keep its context writer after cancellation"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Foreground swarm: two children write the same new path; the parent's
/// turn summary has exactly one net entry for it and the persisted original
/// stays "created" (NULL), with one durable TurnChanges record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_swarm_writes_enter_parent_turn_changes_once() {
    let _test_lock = crate::task::SUBAGENT_SLOT_TEST_LOCK.lock().await;
    let (mut session, parent, root) = swarm_test_session("swarm-turn-changes");
    let call = ToolCall {
        id: "call_swarm_writes".into(),
        name: "AgentSwarm".into(),
        arguments: serde_json::json!({
            "prompt_template": "SUBAGENT_CHILD:WRITE {{item}}",
            "items": ["one", "two"],
        })
        .to_string(),
    };
    let (tx, rx) = async_channel::unbounded();
    let cancel = CancellationToken::new();
    let outcome = session
        .run_swarm(&call, "parent-swarm", &parent, &tx, &cancel)
        .await;
    let SubagentOutcome::Finished { is_error, .. } = outcome else {
        panic!("foreground swarm should finish");
    };
    assert!(!is_error);

    session.flush_turn_changes(&tx);
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let files = events
        .iter()
        .find_map(|event| match event {
            Event::TurnFileChanges { files, .. } => Some(files.clone()),
            _ => None,
        })
        .expect("foreground swarm write should emit TurnFileChanges");
    assert_eq!(files.len(), 1, "one entry per path: {files:?}");
    assert_eq!(files[0].path, "child_write.txt");
    assert!(
        files[0].unified_diff.contains("+x"),
        "the swarm write is in the parent turn summary: {}",
        files[0].unified_diff
    );

    let originals: Vec<(String, Option<String>)> = session
        .store
        .lock()
        .expect("store lock")
        .file_originals(&session.id);
    let matching: Vec<&(String, Option<String>)> = originals
        .iter()
        .filter(|(path, _)| path.ends_with("child_write.txt"))
        .collect();
    assert_eq!(matching.len(), 1, "one original row: {originals:?}");
    assert!(
        matching[0].1.is_none(),
        "the earliest baseline is 'created': {originals:?}"
    );

    let rollout = std::fs::read_to_string(
        root.join("data")
            .join("sessions")
            .join(format!("{}.jsonl", session.id)),
    )
    .expect("rollout readable");
    let records: Vec<serde_json::Value> = rollout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record| record["type"] == "turn_changes")
        .collect();
    assert_eq!(records.len(), 1, "one durable TurnChanges: {rollout}");
    assert_eq!(
        records[0]["files"].as_array().map(Vec::len),
        Some(1),
        "one file in the durable record"
    );
    assert_eq!(
        records[0]["files"][0]["unified_diff"].as_str(),
        Some(files[0].unified_diff.as_str()),
        "durable TurnChanges content matches the live event"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Background child vs parent write on the same existing path: the child
/// snapshots the pre-child content, the parent's later Write sees that
/// shared baseline (not the child's output), its turn summary diffs from
/// the child's output, and revert restores the original file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_child_then_parent_write_preserves_original() {
    let (mut session, parent, root) = swarm_test_session("bg-parent-race");
    let cwd = root.join("ws");
    let path = cwd.join("child_write.txt");
    std::fs::write(&path, "before\n").expect("seed");
    let (tx, rx) = async_channel::unbounded();
    let cancel = CancellationToken::new();

    let read_tool = crate::tool::all()
        .into_iter()
        .find(|tool| tool.name() == "Read")
        .expect("Read tool");
    let write_tool = crate::tool::all()
        .into_iter()
        .find(|tool| tool.name() == "Write")
        .expect("Write tool");
    let read_call = ToolCall {
        id: "parent-read-before-child".into(),
        name: "Read".into(),
        arguments: serde_json::json!({"path": "child_write.txt"}).to_string(),
    };
    assert!(matches!(
        session
            .exec_tool_gated(
                &read_call,
                Some(read_tool.as_ref()),
                "item-read",
                "turn-test",
                &tx,
                &cancel,
            )
            .await,
        GatedToolOutcome::Executed {
            is_error: false,
            ..
        }
    ));

    let call = ToolCall {
        id: "call_bg_race".into(),
        name: "Agent".into(),
        arguments: serde_json::json!({
            "description": "background write race",
            "prompt": "SUBAGENT_CHILD:WRITE seed",
            "subagent_type": "general-purpose",
            "run_in_background": true,
        })
        .to_string(),
    };
    let outcome = session
        .run_subagent(&call, "turn-test", "item-agent", &parent, &tx, &cancel)
        .await;
    assert!(
        matches!(
            outcome,
            SubagentOutcome::Finished {
                is_error: false,
                ..
            }
        ),
        "background child should launch"
    );

    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let originals = session
            .store
            .lock()
            .expect("store lock")
            .file_originals(&session.id);
        if content == "x\n"
            && originals.iter().any(|(original_path, content)| {
                original_path.ends_with("child_write.txt") && content.as_deref() == Some("before\n")
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background child did not finish its write: content={content:?} originals={originals:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    loop {
        let running = session
            .state
            .tasks
            .lock()
            .expect("task registry lock")
            .iter()
            .any(|task| matches!(task.status, pig_protocol::TaskStatus::Running));
        if !running {
            break;
        }
        assert!(Instant::now() < deadline, "background child did not settle");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    session.flush_turn_changes(&tx);
    let mut early_events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        early_events.push(event);
    }
    assert!(
        early_events
            .iter()
            .all(|event| !matches!(event, Event::TurnFileChanges { .. })),
        "a background child must not enter the parent's turn summary: {early_events:#?}"
    );

    let read_call = ToolCall {
        id: "parent-read-after-child".into(),
        name: "Read".into(),
        arguments: serde_json::json!({"path": "child_write.txt"}).to_string(),
    };
    assert!(matches!(
        session
            .exec_tool_gated(
                &read_call,
                Some(read_tool.as_ref()),
                "item-read-2",
                "turn-test",
                &tx,
                &cancel,
            )
            .await,
        GatedToolOutcome::Executed {
            is_error: false,
            ..
        }
    ));
    let write_call = ToolCall {
        id: "parent-write".into(),
        name: "Write".into(),
        arguments: serde_json::json!({
            "path": "child_write.txt",
            "content": "parent\n",
        })
        .to_string(),
    };
    assert!(matches!(
        session
            .exec_tool_gated(
                &write_call,
                Some(write_tool.as_ref()),
                "item-write",
                "turn-test",
                &tx,
                &cancel,
            )
            .await,
        GatedToolOutcome::Executed {
            is_error: false,
            ..
        }
    ));

    session.flush_turn_changes(&tx);
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let files = events
        .iter()
        .find_map(|event| match event {
            Event::TurnFileChanges { files, .. } => Some(files.clone()),
            _ => None,
        })
        .expect("parent write should emit TurnFileChanges");
    assert_eq!(files.len(), 1, "one entry for the path: {files:?}");
    assert_eq!(files[0].path, "child_write.txt");
    assert!(
        files[0].unified_diff.contains("-x") && files[0].unified_diff.contains("+parent"),
        "the parent turn diffs from the child's output: {}",
        files[0].unified_diff
    );
    let rollout = std::fs::read_to_string(
        root.join("data")
            .join("sessions")
            .join(format!("{}.jsonl", session.id)),
    )
    .expect("rollout readable");
    let records: Vec<serde_json::Value> = rollout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record| record["type"] == "turn_changes")
        .collect();
    assert_eq!(records.len(), 1, "one durable TurnChanges: {rollout}");
    assert_eq!(
        records[0]["files"][0]["unified_diff"].as_str(),
        Some(files[0].unified_diff.as_str()),
        "durable TurnChanges content matches the live event"
    );
    let originals: Vec<(String, Option<String>)> = session
        .store
        .lock()
        .expect("store lock")
        .file_originals(&session.id);
    let matching: Vec<&(String, Option<String>)> = originals
        .iter()
        .filter(|(path, _)| path.ends_with("child_write.txt"))
        .collect();
    assert_eq!(matching.len(), 1, "one original row: {originals:?}");
    assert_eq!(
        matching[0].1.as_deref(),
        Some("before\n"),
        "the parent write must keep the earliest baseline: {originals:?}"
    );

    session.revert_file("child_write.txt", &tx);
    assert_eq!(
        std::fs::read_to_string(&path).expect("reverted file"),
        "before\n"
    );
    assert!(
        session
            .store
            .lock()
            .expect("store lock")
            .file_originals(&session.id)
            .iter()
            .all(|(path, _)| !path.ends_with("child_write.txt")),
        "revert clears the baseline row"
    );
    let _ = std::fs::remove_dir_all(root);
}
