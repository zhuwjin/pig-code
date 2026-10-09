mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_turn_with_tool_call() {
    let (config_path, cwd, data_dir) = setup("m2-full");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "read the mock file and summarize".into(),
            files: vec![mock::MOCK_FILE_NAME.into()],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();

    let events = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ReasoningDelta { delta, .. } if !delta.is_empty())),
        "should have reasoning delta"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::TextDelta { .. })),
        "should have text delta"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ToolCallBegin { tool, input_summary, .. }
                if tool == "Read" && input_summary.contains(mock::MOCK_FILE_NAME)
        )),
        "should have Read tool call: {events:#?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. } if output.contains("known file")
        )),
        "tool output should contain the file content"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::MOCK_REPLY_MARKER)
        )),
        "final text should contain the mock marker"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ContextUsage { used: 142, .. })),
        "should have a context usage event"
    );
    assert!(
        events.iter().all(|e| match e {
            Event::TurnStarted {
                session_id: sid, ..
            }
            | Event::ReasoningDelta {
                session_id: sid, ..
            }
            | Event::TextDelta {
                session_id: sid, ..
            }
            | Event::TextDone {
                session_id: sid, ..
            }
            | Event::ToolCallBegin {
                session_id: sid, ..
            }
            | Event::ToolCallEnd {
                session_id: sid, ..
            }
            | Event::ContextUsage {
                session_id: sid, ..
            }
            | Event::TurnComplete {
                session_id: sid, ..
            } => sid == &session_id,
            _ => true,
        }),
        "event session_id should be consistent"
    );

    // The rollout file should already be persisted
    let rollout = data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let content = std::fs::read_to_string(&rollout).expect("rollout exists");
    assert!(content.contains("\"type\":\"meta\""), "first line is meta");
    assert!(content.contains("\"type\":\"user\""));
    assert!(content.contains("\"type\":\"text\""));
    assert!(content.contains("\"type\":\"tool_call\""));

    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_during_stream() {
    let (config_path, cwd, data_dir) = setup("m2-interrupt");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "say something".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();

    recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::ReasoningDelta { .. } | Event::TextDelta { .. })
    })
    .await;
    agent.ops.send(Op::Interrupt { session_id }).await.unwrap();

    let events = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnAborted { .. } | Event::TurnComplete { .. })
    })
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TurnAborted { .. })),
        "should receive TurnAborted: {events:#?}"
    );
    agent.shutdown();
}

/// Interrupt mid-turn after a completed step (its usage already arrived): the
/// cancel wrap-up must still persist the turn's accumulated usage as a
/// turn_stats record (interrupted turns count toward the statistics).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_after_tool_call_persists_turn_stats() {
    let (config_path, cwd, data_dir) = setup("m2-interrupt-stats");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    // Default flow: step 1 is a Read tool call whose final chunk carries
    // usage; step 2 streams the summary text
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "read the mock file and summarize".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::ToolCallEnd { .. })
    })
    .await;
    // Land the interrupt mid-step-2 (after its first delta, before the final
    // usage chunk): the turn aborts with step 1's usage already accumulated
    recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::ReasoningDelta { .. } | Event::TextDelta { .. })
    })
    .await;
    agent
        .ops
        .send(Op::Interrupt {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnAborted { .. })
    })
    .await;
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::TurnAborted { .. })),
        "should receive TurnAborted: {collected:#?}"
    );

    // TurnAborted is emitted before the wrap-up writes; sync with a GetConfig
    // round-trip so the turn has fully returned, then check the rollout
    agent.ops.send(Op::GetConfig).await.unwrap();
    recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::ConfigSnapshot { .. })
    })
    .await;
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .expect("rollout exists");
    assert!(
        rollout.contains("\"type\":\"turn_stats\""),
        "interrupted turn persists its accumulated usage stats: {rollout}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_config_is_empty_not_error() {
    let dir = std::env::temp_dir().join(format!("pig-core-m2-nocfg-{}", std::process::id()));
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(dir.join("nonexistent.toml")),
        dir.clone(),
        dir.join("data"),
    );
    let events = agent.events.clone();

    // A missing config must not error; GetConfig returns an empty snapshot
    agent.ops.send(Op::GetConfig).await.unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::ConfigSnapshot { .. })
    })
    .await;
    assert!(
        !collected.iter().any(|e| matches!(e, Event::Error { .. })),
        "missing config should not error: {collected:#?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::ConfigSnapshot { config, .. } if config.providers.is_empty()
        )),
        "should return an empty config: {collected:#?}"
    );

    // NewSession works normally (model name is the empty-string sentinel; the GUI renders a localized placeholder); only sending reports the structured error
    let sid = new_session(&agent, dir).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid,
            content: "hi".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::Error { .. })
    })
    .await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::Error {
                error: pig_protocol::CoreError::NoModelConfigured,
                ..
            }
        )),
        "sending should report the structured no-model-configured error: {collected:#?}"
    );
    agent.shutdown();
}

/// Send SCENARIO_Q and wait for QuestionRequested, returning the request_id (also asserting on the question content).
async fn wait_question(
    agent: &pig_core::AgentHandle,
    events: &async_channel::Receiver<Event>,
    session_id: &str,
) -> String {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: format!(
                "{} help me decide on an implementation approach",
                mock::SCENARIO_Q_TRIGGER
            ),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    let collected = recv_until(events, Duration::from_secs(20), |e| {
        matches!(e, Event::QuestionRequested { .. })
    })
    .await;
    collected
        .iter()
        .find_map(|e| match e {
            Event::QuestionRequested {
                request_id,
                questions,
                ..
            } => {
                assert_eq!(questions.len(), 2);
                assert_eq!(questions[0].question, "Choose an implementation approach");
                assert_eq!(questions[0].options.len(), 2);
                assert_eq!(questions[1].question, "Should tests run?");
                assert_eq!(questions[1].options.len(), 2);
                Some(request_id.clone())
            }
            _ => None,
        })
        .expect("should receive QuestionRequested")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_user_question_answer() {
    let (config_path, cwd, data_dir) = setup("question-answer");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    let request_id = wait_question(&agent, &events, &session_id).await;
    agent
        .ops
        .send(Op::QuestionReply {
            request_id,
            answers: Some(vec![vec!["Option A".to_string()], vec!["Yes".to_string()]]),
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. }
                if output.contains("The user answered")
                    && output.contains("Option A")
                    && output.contains("Should tests run?: Yes")
        )),
        "tool output should contain both answers: {collected:#?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::MOCK_Q_MARKER)
        )),
        "final text should contain the marker: {collected:#?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_user_question_skip() {
    let (config_path, cwd, data_dir) = setup("question-skip");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    let request_id = wait_question(&agent, &events, &session_id).await;
    agent
        .ops
        .send(Op::QuestionReply {
            request_id,
            answers: None,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. } if output.contains("decide from context")
        )),
        "skipping should tell the model to decide from context: {collected:#?}"
    );
    agent.shutdown();
}

/// Stop during tool execution: the card gets its "Stopped" closing entry
/// persisted to the rollout (not lost on restart replay), the history's
/// tool_use/tool_result pairing stays complete (no dangling entries in the
/// next request), and later turns proceed normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_during_tool_persists_stopped_card() {
    let (config_path, cwd, data_dir) = setup("m2-tool-interrupt");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    // Yolo: sleep 30 skips the approval gate and executes directly
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{} run a slow command", mock::SCENARIO_SLOW_TRIGGER),
            files: vec![],
            images: vec![],
            mode: ExecMode::Yolo,
        })
        .await
        .unwrap();
    recv_until(
        &events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "Bash"),
    )
    .await;
    agent
        .ops
        .send(Op::Interrupt {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();

    let aborted = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnAborted { .. })
    })
    .await;
    // The supplementary ToolCallEnd arrives before TurnAborted; the card settles with "Stopped"
    let end_ix = aborted
        .iter()
        .position(|e| matches!(e, Event::ToolCallEnd { output, is_error: false, .. } if output == "Stopped"))
        .expect("abort should emit the stopped ToolCallEnd");
    let abort_ix = aborted
        .iter()
        .position(|e| matches!(e, Event::TurnAborted { .. }))
        .expect("should receive TurnAborted");
    assert!(
        end_ix < abort_ix,
        "ToolCallEnd should come before TurnAborted"
    );

    // The rollout contains the tool_call record (restart replay rebuilds the card instead of it vanishing)
    let rollout = data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let records = pig_core::rollout::Rollout::load(&rollout).expect("rollout readable");
    assert!(
        records.iter().any(|r| matches!(r,
            pig_core::rollout::RolloutRecord::ToolCall { tool, output, .. }
                if tool == "Bash" && output == "Stopped")),
        "rollout should contain the stopped Bash record: {records:#?}"
    );

    // History rebuild as done on restart: tool_use and tool_result are fully paired (no dangling entries)
    let history = pig_core::rollout::rebuild_history(&records, String::new());
    let tool_use_count = history
        .iter()
        .filter(|m| m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|c| !c.is_empty()))
        .count();
    let tool_result_count = history
        .iter()
        .filter(|m| m.role == "tool" && m.content.as_deref() == Some("Stopped"))
        .count();
    assert_eq!(
        tool_use_count, 1,
        "should have exactly one tool_use: {history:#?}"
    );
    assert_eq!(
        tool_result_count, 1,
        "should have exactly one stopped tool_result"
    );

    // Continue the same session: history stays valid, the next turn completes normally
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "continue".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::Yolo,
        })
        .await
        .unwrap();
    let followup = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    assert!(
        !followup.iter().any(|e| matches!(e, Event::Error { .. })),
        "follow-up after a stop should not error: {followup:#?}"
    );
    agent.shutdown();
}

/// Reasoning ticker demo scenario (TICKER_SCENARIO): the multi-line
/// variable-speed reasoning stream reaches the engine intact — the full
/// reasoning text contains an extra-long line / rapid-fire lines / closing
/// lines, the body carries the marker, and the turn completes normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticker_scenario_streams_multiline_reasoning() {
    let (config_path, cwd, data_dir) = setup("ticker-scenario");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!(
                "{} demo the reasoning ticker",
                mock::SCENARIO_TICKER_TRIGGER
            ),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    // The scenario script runs about 15s in total (deliberately slow); leave ample headroom
    let events = recv_until(&events, Duration::from_secs(60), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    let reasoning: String = events
        .iter()
        .filter_map(|e| match e {
            Event::ReasoningDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    // Multiple lines: all six rapid-fire lines and the three closing lines arrived
    assert!(
        reasoning.lines().count() >= 10,
        "reasoning should have multiple lines: {reasoning}"
    );
    for word in [
        "快速行一",
        "快速行二",
        "快速行三",
        "快速行四",
        "快速行五",
        "快速行六",
    ] {
        assert!(
            reasoning.contains(word),
            "reasoning should contain {word}: {reasoning}"
        );
    }
    // The extra-long line arrives intact (material for the pinned-tail horizontal scroll)
    assert!(
        reasoning.lines().any(|line| line.chars().count() > 100),
        "should have an extra-long reasoning line: {reasoning}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::TICKER_MARKER)
        )),
        "text should contain the marker: {events:#?}"
    );
    agent.shutdown();
}
