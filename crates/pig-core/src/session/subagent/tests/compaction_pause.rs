use super::*;

/// A paused server round that cannot fit is skipped, never dropped: the
/// child keeps its complete paused state and writes no marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_compaction_skips_when_the_paused_pin_cannot_fit() {
    let (session, _parent, root, _log, runtime) =
        compaction_test_session("pin-skip", 16_000, 2_000);
    let mut history = long_child_messages(None, None, None);
    let paused = history.len() - 1;
    let jsonl = seed_child_context(&session, "a9-6", &mut history);
    let mut drive = make_child_session(
        "a9-6".into(),
        root.join("ws"),
        crate::agent::AgentProfile {
            name: "explore".into(),
            description: "pin skip".into(),
            tools: None,
            model: None,
            thought_level: None,
            max_turns: None,
            inject_agents_md: true,
            system_prompt: "test".into(),
            source: crate::agent::AgentSource::BuiltIn,
        },
        runtime.resolve_model(None, None).expect("model"),
        history.clone(),
        jsonl.clone(),
        1,
        "pin skip".into(),
        session.plugin_tools.clone(),
    );
    let before = drive.agent.history.clone();
    let cancel = CancellationToken::new();
    let report = drive
        .compact_child_context(
            CompactTrigger::Watermark,
            &cancel,
            true,
            Some(paused),
            false,
            1,
        )
        .await;
    assert!(matches!(report.outcome, CompactOutcome::Skipped));
    assert_eq!(
        drive.agent.history.len(),
        before.len(),
        "a pinned paused segment must never be dropped"
    );
    assert!(
        compact_lines(&jsonl).is_empty(),
        "a skipped compaction writes no marker"
    );
    drop(session);
    let _ = std::fs::remove_dir_all(root);
}

/// When the active task is omitted and the retained tail starts with an
/// assistant round, the task is preserved separately by index and the
/// omitted/tail counts exclude it (the same Compact replayer contract).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_compaction_preserves_an_omitted_active_task_by_index() {
    let (session, _parent, root, _log, runtime) =
        compaction_test_session("preserved-user", 30_000, 2_000);
    let call = ToolCall {
        id: "call_huge".into(),
        name: "Read".into(),
        arguments: "{\"path\":\"huge.txt\"}".into(),
    };
    let history = vec![
        ChatMsg::system("child system".into()),
        ChatMsg::user("active task brief".into()),
        ChatMsg::assistant(String::new(), vec![call], None),
        ChatMsg::tool_result("call_huge", "h".repeat(50_000)),
        ChatMsg::assistant("later small round".into(), vec![], None),
    ];
    let mut seeded = history.clone();
    let jsonl = seed_child_context(&session, "a9-9", &mut seeded);
    let mut drive = make_child_session(
        "a9-9".into(),
        root.join("ws"),
        crate::agent::AgentProfile {
            name: "explore".into(),
            description: "preserved user".into(),
            tools: None,
            model: None,
            thought_level: None,
            max_turns: None,
            inject_agents_md: true,
            system_prompt: "test".into(),
            source: crate::agent::AgentSource::BuiltIn,
        },
        runtime.resolve_model(None, None).expect("model"),
        history,
        jsonl.clone(),
        1,
        "preserved user".into(),
        session.plugin_tools.clone(),
    );
    let cancel = CancellationToken::new();
    let report = drive
        .compact_child_context(CompactTrigger::Watermark, &cancel, false, None, false, 1)
        .await;
    assert!(matches!(report.outcome, CompactOutcome::Completed));
    assert_eq!(
        drive
            .agent
            .history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user", "user", "assistant"],
        "the active user is installed after the note, then the assistant tail"
    );
    assert_eq!(
        drive.agent.history[2].content.as_deref(),
        Some("active task brief")
    );
    assert_eq!(
        drive.agent.history[3].content.as_deref(),
        Some("later small round")
    );
    let compacts = compact_lines(&jsonl);
    assert_eq!(compacts.len(), 1, "{compacts:?}");
    assert_eq!(compacts[0]["omitted"], 2, "{:?}", compacts[0]);
    assert_eq!(compacts[0]["tail_len"], 1, "{:?}", compacts[0]);
    assert_eq!(
        compacts[0]["preserved_user"], 1,
        "the active task is preserved by its original-history index: {:?}",
        compacts[0]
    );
    // Restart replay produces the same preserved user and tail.
    let (_, replayed) = crate::agent::read_agent(&jsonl).expect("replay compacted context");
    assert_eq!(replayed.len(), 4);
    assert_eq!(replayed[2].content.as_deref(), Some("active task brief"));
    assert_eq!(replayed[3].content.as_deref(), Some("later small round"));
    drop(session);
    let _ = std::fs::remove_dir_all(root);
}

/// A single oversized assistant round leaves a zero-length raw tail: the
/// active task is installed after the note by index, and replay keeps the
/// same request anchor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_compaction_preserves_the_task_with_a_zero_length_tail() {
    let (session, _parent, root, _log, runtime) =
        compaction_test_session("zero-tail", 30_000, 2_000);
    let history = vec![
        ChatMsg::system("child system".into()),
        ChatMsg::user("active task brief".into()),
        ChatMsg::assistant(
            format!("oversized answer {}", "z".repeat(100_000)),
            vec![],
            None,
        ),
    ];
    let mut seeded = history.clone();
    let jsonl = seed_child_context(&session, "a9-10", &mut seeded);
    let mut drive = make_child_session(
        "a9-10".into(),
        root.join("ws"),
        crate::agent::AgentProfile {
            name: "explore".into(),
            description: "zero tail".into(),
            tools: None,
            model: None,
            thought_level: None,
            max_turns: None,
            inject_agents_md: true,
            system_prompt: "test".into(),
            source: crate::agent::AgentSource::BuiltIn,
        },
        runtime.resolve_model(None, None).expect("model"),
        history,
        jsonl.clone(),
        1,
        "zero tail".into(),
        session.plugin_tools.clone(),
    );
    let cancel = CancellationToken::new();
    let report = drive
        .compact_child_context(CompactTrigger::Overflow, &cancel, false, None, false, 1)
        .await;
    assert!(matches!(report.outcome, CompactOutcome::Completed));
    assert_eq!(
        drive
            .agent
            .history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user", "user"]
    );
    assert_eq!(
        drive.agent.history[2].content.as_deref(),
        Some("active task brief")
    );
    let compacts = compact_lines(&jsonl);
    assert_eq!(compacts.len(), 1, "{compacts:?}");
    assert_eq!(compacts[0]["omitted"], 1, "{:?}", compacts[0]);
    assert_eq!(compacts[0]["tail_len"], 0, "{:?}", compacts[0]);
    assert_eq!(compacts[0]["preserved_user"], 1, "{:?}", compacts[0]);
    let (_, replayed) = crate::agent::read_agent(&jsonl).expect("replay zero-tail context");
    assert_eq!(
        replayed
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user", "user"]
    );
    assert_eq!(replayed[2].content.as_deref(), Some("active task brief"));
    drop(session);
    let _ = std::fs::remove_dir_all(root);
}

/// The child summary sidecar goes through the same provider format
/// encoders as root compaction: the mock validates the Anthropic request
/// shape, and all three formats must return a real summary (not fall back).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_compaction_summary_sidecar_supports_all_api_formats() {
    let (session, _parent, root, _log, runtime) = compaction_test_session("formats", 30_000, 2_000);
    let base = runtime.resolve_model(None, None).expect("model");
    let call = ToolCall {
        id: "call_format".into(),
        name: "Read".into(),
        arguments: "{\"path\":\"format.txt\"}".into(),
    };
    let history = vec![
        ChatMsg::system("child system".into()),
        ChatMsg::user("format task".into()),
        ChatMsg::assistant(String::new(), vec![call], None),
        ChatMsg::tool_result("call_format", "f".repeat(50_000)),
        ChatMsg::assistant("small tail".into(), vec![], None),
    ];
    for (ix, format) in [
        pig_protocol::ApiFormat::OpenAiChat,
        pig_protocol::ApiFormat::OpenAiResponses,
        pig_protocol::ApiFormat::AnthropicMessages,
    ]
    .into_iter()
    .enumerate()
    {
        let agent_id = format!("a9-2{ix}");
        let mut seeded = history.clone();
        let jsonl = seed_child_context(&session, &agent_id, &mut seeded);
        let mut child_config = base.clone();
        child_config.api_format = format;
        let mut drive = make_child_session(
            &agent_id,
            root.join("ws"),
            crate::agent::AgentProfile {
                name: "explore".into(),
                description: "formats".into(),
                tools: None,
                model: None,
                thought_level: None,
                max_turns: None,
                inject_agents_md: true,
                system_prompt: "test".into(),
                source: crate::agent::AgentSource::BuiltIn,
            },
            child_config,
            history.clone(),
            jsonl.clone(),
            1,
            "formats".into(),
            session.plugin_tools.clone(),
        );
        let cancel = CancellationToken::new();
        let report = drive
            .compact_child_context(CompactTrigger::Watermark, &cancel, false, None, false, 1)
            .await;
        assert!(
            matches!(report.outcome, CompactOutcome::Completed),
            "{format:?} compaction should complete"
        );
        let compacts = compact_lines(&jsonl);
        assert_eq!(compacts.len(), 1, "{format:?}: {compacts:?}");
        assert!(
            compacts[0]["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains(pig_provider::mock::SUMMARY_MARKER)),
            "{format:?} summary sidecar failed: {:?}",
            compacts[0]
        );
        assert_eq!(
            drive
                .agent
                .history
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["system", "user", "user", "assistant"],
            "{format:?} preserved the active task and tail"
        );
    }
    drop(session);
    let _ = std::fs::remove_dir_all(root);
}

/// A pause continuation retains the complete pinned paused segment and a
/// later steer while omitting older work; the paused assistant index is
/// remapped so a following recovery still pins the real server state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_compaction_keeps_a_pinned_paused_round_and_steer() {
    let (session, _parent, root, _log, runtime) = compaction_test_session("pin-fit", 30_000, 2_000);
    let paused_call = ToolCall {
        id: "call_paused".into(),
        name: "Read".into(),
        arguments: "{\"path\":\"paused.txt\"}".into(),
    };
    let history = vec![
        ChatMsg::system("child system".into()),
        ChatMsg::user(format!("huge old work {}", "a".repeat(100_000))),
        ChatMsg::assistant(format!("old done {}", "a".repeat(100_000)), vec![], None),
        ChatMsg::user("paused task".into()),
        ChatMsg::assistant(String::new(), vec![paused_call], None),
        ChatMsg::tool_result("call_paused", "paused tool output".into()),
        ChatMsg::user("steer after pause".into()),
    ];
    // The driver pins the last message of the paused round (the tool
    // result); the whole user segment must survive, pairs intact.
    let pin = 5;
    let mut seeded = history.clone();
    let jsonl = seed_child_context(&session, "a9-8", &mut seeded);
    let mut drive = make_child_session(
        "a9-8".into(),
        root.join("ws"),
        crate::agent::AgentProfile {
            name: "explore".into(),
            description: "pin fit".into(),
            tools: None,
            model: None,
            thought_level: None,
            max_turns: None,
            inject_agents_md: true,
            system_prompt: "test".into(),
            source: crate::agent::AgentSource::BuiltIn,
        },
        runtime.resolve_model(None, None).expect("model"),
        history,
        jsonl.clone(),
        1,
        "pin fit".into(),
        session.plugin_tools.clone(),
    );
    let cancel = CancellationToken::new();
    let report = drive
        .compact_child_context(
            CompactTrigger::Watermark,
            &cancel,
            true,
            Some(pin),
            false,
            1,
        )
        .await;
    assert!(matches!(report.outcome, CompactOutcome::Completed));
    assert_eq!(
        report.pin_after,
        Some(4),
        "the paused round's last message is remapped into the rewritten history"
    );
    assert_eq!(
        drive
            .agent
            .history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user", "user", "assistant", "tool", "user"]
    );
    assert_eq!(
        drive.agent.history[2].content.as_deref(),
        Some("paused task")
    );
    assert_eq!(
        drive.agent.history[3]
            .tool_calls
            .as_ref()
            .expect("the paused assistant tool call survives")
            .len(),
        1
    );
    assert_eq!(
        drive.agent.history[3].tool_calls.as_ref().unwrap()[0].id,
        "call_paused"
    );
    assert_eq!(
        drive.agent.history[4].tool_call_id.as_deref(),
        Some("call_paused"),
        "the tool pair stays complete and adjacent"
    );
    assert_eq!(
        drive.agent.history[4].content.as_deref(),
        Some("paused tool output")
    );
    assert_eq!(
        drive.agent.history[5].content.as_deref(),
        Some("steer after pause"),
        "the steer drained after the pause stays in context"
    );
    let compacts = compact_lines(&jsonl);
    assert_eq!(compacts.len(), 1, "{compacts:?}");
    assert_eq!(compacts[0]["tail_len"], 4, "{:?}", compacts[0]);
    assert!(
        compacts[0]["preserved_user"].is_null(),
        "the active task is inside the retained tail: {:?}",
        compacts[0]
    );
    drop(session);
    let _ = std::fs::remove_dir_all(root);
}

/// Cancellation during the summary sidecar cannot install a late summary
/// or compact marker: the paused/cancelled driver keeps its context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_child_compaction_does_not_install_a_late_summary() {
    let (session, _parent, root, _log, runtime) =
        compaction_test_session("cancel-summary", 36_000, 2_000);
    let mut history = long_child_messages(
        None,
        None,
        Some(pig_provider::mock::SIDECAR_STALL_BODY_TRIGGER),
    );
    let jsonl = seed_child_context(&session, "a9-7", &mut history);
    let mut drive = make_child_session(
        "a9-7".into(),
        root.join("ws"),
        crate::agent::AgentProfile {
            name: "explore".into(),
            description: "cancel summary".into(),
            tools: None,
            model: None,
            thought_level: None,
            max_turns: None,
            inject_agents_md: true,
            system_prompt: "test".into(),
            source: crate::agent::AgentSource::BuiltIn,
        },
        runtime.resolve_model(None, None).expect("model"),
        history.clone(),
        jsonl.clone(),
        1,
        "cancel summary".into(),
        session.plugin_tools.clone(),
    );
    let before = drive.agent.history.clone();
    let cancel = CancellationToken::new();
    let cancel_task = cancel.clone();
    let handle = tokio::spawn(async move {
        let report = drive
            .compact_child_context(
                CompactTrigger::Watermark,
                &cancel_task,
                false,
                None,
                false,
                1,
            )
            .await;
        (report, drive.agent.history)
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    cancel.cancel();
    let (report, history_after) = handle.await.expect("compaction task");
    assert!(matches!(report.outcome, CompactOutcome::Cancelled));
    assert_eq!(
        history_after.len(),
        before.len(),
        "a cancelled summary must not rewrite the child context"
    );
    assert!(
        compact_lines(&jsonl).is_empty(),
        "a cancelled summary must not append a compact marker"
    );
    drop(session);
    let _ = std::fs::remove_dir_all(root);
}
