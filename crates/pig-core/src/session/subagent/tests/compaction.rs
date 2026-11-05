use super::*;

/// A long single-task child compacts at preflight (foreground/resume),
/// continues on the same turn, and a restart replays the compacted prefix
/// from the child JSONL instead of the omitted work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_resume_compacts_a_long_child_and_restart_replays_it() {
    let _test_lock = crate::task::SUBAGENT_SLOT_TEST_LOCK.lock().await;
    let (mut session, parent, root, log, runtime) =
        compaction_test_session("foreground", 30_000, 2_000);
    let mut history = long_child_messages(
        None,
        Some(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        None,
    );
    let jsonl = seed_child_context(&session, "a9-1", &mut history);
    let (tx, _rx) = async_channel::unbounded();
    let cancel = CancellationToken::new();
    let call = child_resume_call("call_resume_1", "a9-1", "first", false);
    let outcome = session
        .run_subagent(&call, "turn", "item-agent", &parent, &tx, &cancel)
        .await;
    let SubagentOutcome::Finished {
        note,
        is_error,
        card,
        ..
    } = outcome
    else {
        panic!("foreground resume should finish");
    };
    assert!(!is_error, "{note}");
    assert!(
        note.contains(pig_provider::mock::SUBAGENT_RESUMED_DONE),
        "{note}"
    );
    assert!(note.contains("agent_id: a9-1"), "{note}");
    assert!(card.is_some());

    let compacts = compact_lines(&jsonl);
    assert_eq!(
        compacts.len(),
        1,
        "one canonical child compaction: {compacts:?}"
    );
    let compact = &compacts[0];
    assert!(
        compact["omitted"]
            .as_u64()
            .is_some_and(|omitted| omitted > 0),
        "{compact}"
    );
    assert!(
        compact["tail_len"].as_u64().is_some_and(|tail| tail > 0),
        "{compact}"
    );
    assert_eq!(compact["trigger"], "watermark", "{compact}");
    assert!(
        compact["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains(pig_provider::mock::SUMMARY_MARKER)),
        "the summary sidecar supplied the note: {compact}"
    );
    assert!(
        compact["note"]
            .as_str()
            .is_some_and(|note| note.contains("Earlier context compacted")),
        "{compact}"
    );
    assert!(
        !serde_json::to_string(compact)
            .unwrap()
            .contains("data_base64"),
        "the marker carries no base64: {compact}"
    );
    let requests = child_stream_requests(&log, "SUBAGENT_CHILD:RESUMED first");
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let request = requests[0].to_string();
    assert!(request.contains("Earlier context compacted"), "{request}");
    assert!(
        request.contains(pig_provider::mock::SUMMARY_MARKER),
        "{request}"
    );
    assert!(
        !request.contains(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        "the omitted round left the retried request: {request}"
    );
    let summaries = summary_requests(&log);
    assert!(!summaries.is_empty(), "a summary sidecar ran");
    assert!(
        summaries.len() <= 2,
        "the summary retry is bounded: {}",
        summaries.len()
    );
    let summary_triggers: Vec<bool> = summaries
        .iter()
        .map(|body| body.contains(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER))
        .collect();
    assert!(
        !summary_triggers.last().copied().unwrap_or(false),
        "the last summary attempt dropped the overflow trigger: attempts={} triggers={summary_triggers:?} compact={:?}",
        summaries.len(),
        compacts[0]
    );

    // Restart: load the session from disk and resume again. The compacted
    // prefix is rebuilt from the child JSONL (no second compaction, no
    // omitted trigger, same note).
    drop(session);
    let data_dir = root.join("data");
    let store = Arc::new(Mutex::new(
        crate::store::Store::open(&data_dir).expect("restart store"),
    ));
    let (task_tx, _task_rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut session, _records) = Session::load(
        "s-child-compact-foreground",
        &data_dir.join("sessions"),
        test_pending(),
        Arc::new(Mutex::new(HashMap::new())),
        store,
        data_dir.clone(),
        task_tx,
        &crate::plugins::PluginRegistry::shared(data_dir),
        Some(runtime),
    )
    .expect("load session");
    session.set_permissions(pig_protocol::ExecMode::Auto.permissions());
    let (tx, _rx) = async_channel::unbounded();
    let cancel = CancellationToken::new();
    let call = child_resume_call("call_resume_2", "a9-1", "restart", false);
    let outcome = session
        .run_subagent(&call, "turn", "item-agent", &parent, &tx, &cancel)
        .await;
    let SubagentOutcome::Finished { note, is_error, .. } = outcome else {
        panic!("restart resume should finish");
    };
    assert!(!is_error, "{note}");
    assert!(
        note.contains(pig_provider::mock::SUBAGENT_RESUMED_DONE),
        "{note}"
    );
    assert_eq!(
        compact_lines(&jsonl).len(),
        1,
        "restart replay must not compact the already-compacted context again"
    );
    let requests = child_stream_requests(&log, "SUBAGENT_CHILD:RESUMED restart");
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let request = requests[0].to_string();
    assert!(request.contains("Earlier context compacted"), "{request}");
    assert!(
        request.contains(pig_provider::mock::SUMMARY_MARKER),
        "{request}"
    );
    assert!(
        !request.contains(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        "{request}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Preflight child compaction and its streaming continuation on the Responses
/// and Anthropic wire formats: the compacted request carries the note and the
/// sidecar summary in that format's shape, the omitted overflow round is gone,
/// and exactly one canonical compact marker is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_resume_compaction_streams_on_all_api_formats() {
    let _test_lock = crate::task::SUBAGENT_SLOT_TEST_LOCK.lock().await;
    for (format, tag) in [
        (pig_protocol::ApiFormat::OpenAiResponses, "responses"),
        (pig_protocol::ApiFormat::AnthropicMessages, "anthropic"),
    ] {
        let (mut session, parent, root, log, _runtime) =
            compaction_test_session_with_format(tag, 30_000, 2_000, format);
        let mut history = long_child_messages(
            None,
            Some(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
            None,
        );
        let jsonl = seed_child_context(&session, "a9-1", &mut history);
        let (tx, _rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let call = child_resume_call(&format!("call_resume_{tag}"), "a9-1", "first", false);
        let outcome = session
            .run_subagent(&call, "turn", "item-agent", &parent, &tx, &cancel)
            .await;
        let SubagentOutcome::Finished { note, is_error, .. } = outcome else {
            panic!("{tag}: foreground resume should finish");
        };
        assert!(!is_error, "{tag}: {note}");
        assert!(
            note.contains(pig_provider::mock::SUBAGENT_RESUMED_DONE),
            "{tag}: {note}"
        );

        let compacts = compact_lines(&jsonl);
        assert_eq!(compacts.len(), 1, "{tag}: {compacts:?}");
        assert!(
            compacts[0]["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains(pig_provider::mock::SUMMARY_MARKER)),
            "{tag}: the summary sidecar supplied the note: {:?}",
            compacts[0]
        );

        let requests =
            child_stream_requests_for_format(&log, "SUBAGENT_CHILD:RESUMED first", format);
        assert_eq!(requests.len(), 1, "{tag}: {requests:#?}");
        let request = requests[0].to_string();
        assert!(
            request.contains("Earlier context compacted"),
            "{tag}: {request}"
        );
        assert!(
            request.contains(pig_provider::mock::SUMMARY_MARKER),
            "{tag}: {request}"
        );
        assert!(
            !request.contains(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
            "{tag}: the omitted round left the retried request: {request}"
        );
        assert!(
            !summary_requests(&log).is_empty(),
            "{tag}: a summary sidecar ran"
        );

        drop(session);
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Background Agent resume uses the same managed loop and preflight
/// compaction; the child settles normally and its marker is durable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_resume_compacts_a_long_child() {
    let _test_lock = crate::task::SUBAGENT_SLOT_TEST_LOCK.lock().await;
    let (mut session, parent, root, log, _runtime) =
        compaction_test_session("background", 30_000, 2_000);
    let mut history = long_child_messages(
        None,
        Some(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        None,
    );
    let jsonl = seed_child_context(&session, "a9-2", &mut history);
    let (tx, _rx) = async_channel::unbounded();
    let cancel = CancellationToken::new();
    let call = child_resume_call("call_resume_bg", "a9-2", "background", true);
    let outcome = session
        .run_subagent(&call, "turn", "item-agent", &parent, &tx, &cancel)
        .await;
    assert!(
        matches!(
            outcome,
            SubagentOutcome::Finished {
                is_error: false,
                ..
            }
        ),
        "background resume should launch"
    );
    wait_for_background_child(&session).await;
    let tasks = child_task_statuses(&session);
    assert!(
        tasks.iter().all(|(_, status)| !matches!(
            status,
            pig_protocol::TaskStatus::Queued | pig_protocol::TaskStatus::Running
        )),
        "{tasks:?}"
    );
    assert_eq!(compact_lines(&jsonl).len(), 1, "background child compacts");
    let requests = child_stream_requests(&log, "SUBAGENT_CHILD:RESUMED background");
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let request = requests[0].to_string();
    assert!(request.contains("Earlier context compacted"), "{request}");
    assert!(
        !request.contains(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        "{request}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Foreground swarm resume goes through the same loop and compacts the
/// resumed child's long context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_resume_compacts_a_long_child() {
    let _test_lock = crate::task::SUBAGENT_SLOT_TEST_LOCK.lock().await;
    let (mut session, parent, root, log, _runtime) =
        compaction_test_session("swarm", 30_000, 2_000);
    let mut history = long_child_messages(
        None,
        Some(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        None,
    );
    let jsonl = seed_child_context(&session, "a9-3", &mut history);
    let call = ToolCall {
        id: "call_swarm_resume".into(),
        name: "AgentSwarm".into(),
        arguments: serde_json::json!({
            "prompt_template": "unused for resume-only batches",
            "resume_agent_ids": {
                "a9-3": "SUBAGENT_CHILD:RESUMED swarm",
            },
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
        panic!("swarm resume should finish");
    };
    assert!(!is_error, "{note}");
    assert!(
        note.contains(pig_provider::mock::SUBAGENT_RESUMED_DONE),
        "{note}"
    );
    assert!(
        note.contains("1 subagents total (1 completed / 0 failed / 0 cancelled)"),
        "{note}"
    );
    assert_eq!(cards.len(), 1);
    assert_eq!(compact_lines(&jsonl).len(), 1, "swarm child compacts");
    let requests = child_stream_requests(&log, "SUBAGENT_CHILD:RESUMED swarm");
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let request = requests[0].to_string();
    assert!(request.contains("Earlier context compacted"), "{request}");
    assert!(
        !request.contains(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        "{request}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Typical typed-overflow recovery: preflight is under the trigger, the
/// first request is rejected by the provider, one overflow compaction
/// omits the offending round, and the retried step completes. The summary
/// sidecar's own typed overflow deliberately falls back to truncation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reactive_overflow_compacts_and_retries_the_child_step_once() {
    let _test_lock = crate::task::SUBAGENT_SLOT_TEST_LOCK.lock().await;
    let (mut session, parent, root, log, _runtime) =
        compaction_test_session("reactive", 100_000, 2_000);
    let mut history = long_child_messages(
        None,
        Some(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        None,
    );
    let jsonl = seed_child_context(&session, "a9-4", &mut history);
    let (tx, _rx) = async_channel::unbounded();
    let cancel = CancellationToken::new();
    let call = child_resume_call("call_resume_reactive", "a9-4", "reactive", false);
    let outcome = session
        .run_subagent(&call, "turn", "item-agent", &parent, &tx, &cancel)
        .await;
    let SubagentOutcome::Finished { note, is_error, .. } = outcome else {
        panic!("reactive recovery should finish");
    };
    assert!(!is_error, "{note}");
    assert!(
        note.contains(pig_provider::mock::SUBAGENT_RESUMED_DONE),
        "{note}"
    );
    let compacts = compact_lines(&jsonl);
    assert_eq!(compacts.len(), 1, "{compacts:?}");
    assert_eq!(compacts[0]["trigger"], "overflow", "{:?}", compacts[0]);
    assert!(
        compacts[0]["note"]
            .as_str()
            .is_some_and(|note| note.contains("summary generation failed")),
        "the bounded summary failure fell back deliberately: {:?}",
        compacts[0]
    );
    let requests = child_stream_requests(&log, "SUBAGENT_CHILD:RESUMED reactive");
    assert_eq!(
        requests.len(),
        2,
        "failed attempt + retried step: {requests:#?}"
    );
    let retried = requests[1].to_string();
    assert!(retried.contains("Earlier context compacted"), "{retried}");
    assert!(
        !retried.contains(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        "{retried}"
    );
    let summaries = summary_requests(&log);
    assert!(
        summaries.len() <= 2,
        "the summary retry stays bounded: {}",
        summaries.len()
    );
    let _ = std::fs::remove_dir_all(root);
}

/// An overflow that no compaction can remove (the trigger is in the
/// frozen system prompt) is surfaced once with the child agent_id and a
/// concrete resume path; it never retries forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrecoverable_child_overflow_keeps_agent_id_and_resume_guidance() {
    let _test_lock = crate::task::SUBAGENT_SLOT_TEST_LOCK.lock().await;
    let (mut session, parent, root, log, _runtime) =
        compaction_test_session("unrecoverable", 100_000, 2_000);
    let mut history = long_child_messages(
        Some(pig_provider::mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        None,
        None,
    );
    let jsonl = seed_child_context(&session, "a9-5", &mut history);
    let (tx, _rx) = async_channel::unbounded();
    let cancel = CancellationToken::new();
    let call = child_resume_call("call_resume_unrecoverable", "a9-5", "unrecoverable", false);
    let outcome = session
        .run_subagent(&call, "turn", "item-agent", &parent, &tx, &cancel)
        .await;
    let SubagentOutcome::Finished { note, is_error, .. } = outcome else {
        panic!("unrecoverable overflow should settle as a failure");
    };
    assert!(is_error, "{note}");
    assert!(note.contains("agent_id: a9-5"), "{note}");
    assert!(note.contains("exceeded its model context window"), "{note}");
    assert!(
        note.contains("Agent(resume=\"a9-5\""),
        "actionable resume guidance: {note}"
    );
    assert!(note.contains("larger context window"), "{note}");
    let requests = child_stream_requests(&log, "SUBAGENT_CHILD:RESUMED unrecoverable");
    assert_eq!(
        requests.len(),
        2,
        "one failed request + at most one bounded recovery retry: {requests:#?}"
    );
    assert_eq!(
        compact_lines(&jsonl).len(),
        1,
        "the recovery compaction is durable"
    );
    let _ = std::fs::remove_dir_all(root);
}
