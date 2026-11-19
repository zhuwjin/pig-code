//! Agent subagent tool (A2a foreground sync) integration tests: happy path /
//! tool narrowing / approval gate / max_turns / strict model failure / result
//! truncation spill / Plan mode rejection.

use crate::common;

use common::{new_session, recv_until, setup, setup_with_log};
use pig_protocol::{ApprovalDecision, Event, ExecMode, Op};
use pig_provider::mock;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Send one message and collect until the turn ends; when `approve` is given, approval requests are answered automatically with that decision.
async fn run_turn(
    agent: &pig_core::AgentHandle,
    sid: &str,
    text: &str,
    mode: ExecMode,
    approve: Option<ApprovalDecision>,
) -> Vec<Event> {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.into(),
            content: text.into(),
            files: vec![],
            images: vec![],
            permissions: mode.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the turn to end: {collected:#?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event
            && let Some(decision) = approve
        {
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision,
                    feedback: None,
                })
                .await
                .unwrap();
        }
        let done = matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. }
        );
        collected.push(event);
        if done {
            break;
        }
    }
    collected
}

/// (output, is_error) of the Agent tool card (item_id contains the subagent call's fixed id)
fn agent_end(events: &[Event]) -> Option<(&str, bool)> {
    events.iter().find_map(|e| match e {
        Event::ToolCallEnd {
            item_id,
            output,
            is_error,
            ..
        } if item_id.contains("call_agent_1") => Some((output.as_str(), *is_error)),
        _ => None,
    })
}

/// Parse the agent_id line from the Agent result text
fn parse_agent_id(output: &str) -> &str {
    output
        .lines()
        .find_map(|line| line.strip_prefix("agent_id: "))
        .expect("result should have an agent_id line")
}

/// Subagent context JSONL path: {data}/sessions/{sid}.agents/{agent_id}.jsonl
fn agent_log_path(data_dir: &Path, sid: &str, agent_id: &str) -> PathBuf {
    data_dir
        .join("sessions")
        .join(format!("{sid}.agents"))
        .join(format!("{agent_id}.jsonl"))
}

/// Per-turn file-change panel rows carried by one turn's events.
fn turn_file_changes(events: &[Event]) -> Vec<pig_protocol::EditDiff> {
    events
        .iter()
        .find_map(|event| match event {
            Event::TurnFileChanges { files, .. } => Some(files.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// `file_originals` contents for paths ending with `suffix` (None = the file
/// did not exist when the session first tracked it).
fn file_originals(data_dir: &Path, sid: &str, suffix: &str) -> Vec<Option<String>> {
    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).unwrap();
    let mut stmt = db
        .prepare(
            "SELECT content FROM file_originals
             WHERE session_id = ?1 AND path LIKE ?2 ORDER BY path",
        )
        .unwrap();
    stmt.query_map(rusqlite::params![sid, format!("%{suffix}")], |row| {
        row.get::<_, Option<String>>(0)
    })
    .unwrap()
    .map(|row| row.unwrap())
    .collect()
}

fn pdf_fixture() -> Vec<u8> {
    b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n".to_vec()
}

/// A real minimal PNG: the child replay now validates persisted image bytes
/// through the owning media root, so a fake byte string degrades as corrupt.
fn png_fixture() -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

/// Seed a resumable subagent context whose tool result references a real PDF.
/// Mirrors `append_agent_message`'s on-disk shape (document refs live beside
/// the JSONL, never base64). Returns the persisted media path.
fn seed_pdf_agent(data_dir: &Path, sid: &str, agent_id: &str, pdf: &[u8]) -> PathBuf {
    let jsonl = agent_log_path(data_dir, sid, agent_id);
    std::fs::create_dir_all(jsonl.parent().expect("agent dir")).unwrap();
    let media_dir = jsonl.with_extension("media");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media_path = media_dir.join("1.pdf");
    std::fs::write(&media_path, pdf).unwrap();
    let call = pig_provider::ToolCall {
        id: "call_pdf".into(),
        name: "ReadMediaFile".into(),
        arguments: "{\"path\":\"doc.pdf\"}".into(),
    };
    let records = [
        pig_core::rollout::RolloutRecord::subagent_meta(
            agent_id.into(),
            sid.into(),
            data_dir.to_path_buf(),
            "pdf resume fixture".into(),
            "explore".into(),
            "mock".into(),
            "mock-model".into(),
            1,
            1,
        ),
        pig_core::rollout::RolloutRecord::System {
            text: "You are a subagent.".into(),
        },
        pig_core::rollout::RolloutRecord::Assistant {
            text: String::new(),
            reasoning: None,
            replay: None,
            tool_calls: vec![call.to_wire()],
            tool_execution_ids: vec!["execution-test".into()],
            ts_ms: 1,
            reasoning_ms: None,
        },
        pig_core::rollout::RolloutRecord::ToolCall {
            execution_id: "execution-test".into(),
            tool: call.name.clone(),
            summary: "Read PDF doc.pdf".into(),
            arguments: call.arguments.clone(),
            output: "Read PDF doc.pdf".into(),
            is_error: false,
            stopped: false,
            edit: None,
            agent_card: None,
            agent_cards: Vec::new(),
            images: Vec::new(),
            documents: vec![pig_core::rollout::DocumentRef {
                path: media_path.clone(),
                media_type: "application/pdf".into(),
                bytes: pdf.len() as u64,
                token_estimate: None,
                label: Some("doc.pdf".into()),
            }],
            delivered_notification_ids: Vec::new(),
            recovery: None,
        },
    ];
    let lines: Vec<String> = records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect();
    std::fs::write(
        &jsonl,
        format!(
            "{}
",
            lines.join(
                "
"
            )
        ),
    )
    .unwrap();
    media_path
}

/// Seed a resumable subagent context whose tool result references a real
/// image file (JSONL stores the ref, not base64).
fn seed_image_agent(data_dir: &Path, sid: &str, agent_id: &str) -> PathBuf {
    let jsonl = agent_log_path(data_dir, sid, agent_id);
    std::fs::create_dir_all(jsonl.parent().expect("agent dir")).unwrap();
    let media_dir = jsonl.with_extension("media");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media_path = media_dir.join("1.png");
    std::fs::write(&media_path, png_fixture()).unwrap();
    let call = pig_provider::ToolCall {
        id: "call_image".into(),
        name: "ReadMediaFile".into(),
        arguments: "{\"path\":\"pic.png\"}".into(),
    };
    let records = [
        pig_core::rollout::RolloutRecord::subagent_meta(
            agent_id.into(),
            sid.into(),
            data_dir.to_path_buf(),
            "image resume fixture".into(),
            "explore".into(),
            "mock".into(),
            "mock-model".into(),
            1,
            1,
        ),
        pig_core::rollout::RolloutRecord::System {
            text: "You are a subagent.".into(),
        },
        pig_core::rollout::RolloutRecord::Assistant {
            text: String::new(),
            reasoning: None,
            replay: None,
            tool_calls: vec![call.to_wire()],
            tool_execution_ids: vec!["execution-test".into()],
            ts_ms: 1,
            reasoning_ms: None,
        },
        pig_core::rollout::RolloutRecord::ToolCall {
            execution_id: "execution-test".into(),
            tool: call.name.clone(),
            summary: "Read image pic.png".into(),
            arguments: call.arguments.clone(),
            output: "Read image pic.png".into(),
            is_error: false,
            stopped: false,
            edit: None,
            agent_card: None,
            agent_cards: Vec::new(),
            images: vec![pig_core::rollout::ImageRef {
                path: media_path.clone(),
                media_type: "image/png".into(),
                width: 1,
                height: 1,
                label: Some("pic.png".into()),
            }],
            documents: Vec::new(),
            delivered_notification_ids: Vec::new(),
            recovery: None,
        },
    ];
    let lines: Vec<String> = records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect();
    std::fs::write(
        &jsonl,
        format!(
            "{}
",
            lines.join(
                "
"
            )
        ),
    )
    .unwrap();
    media_path
}

fn resumed_child_requests(log: &pig_provider::mock::RequestLog) -> Vec<serde_json::Value> {
    log.lock()
        .expect("request log")
        .iter()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .filter(|request| {
            request["stream"] == true
                && request.to_string().contains("SUBAGENT_CHILD:RESUMED")
                && request["messages"][0]["content"]
                    .as_str()
                    .is_some_and(|system| !system.contains("Pig Code"))
        })
        .collect()
}

/// Happy path: explore delegation (child calls Grep → child text wrap-up).
/// The Agent result brings back the child conclusion + agent_id + resume_hint +
/// status: completed; the parent rollout has only the single Agent tool
/// record; the subagent context is persisted; progress events go through
/// SubagentProgress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_happy_path() {
    let (config_path, cwd, data_dir) = setup("subagent-happy");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;

    // Agent tool card: success, brings back the child conclusion and template fields
    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(!is_error, "happy path should not fail: {output}");
    assert!(
        output.contains(mock::SUBAGENT_CHILD_DONE),
        "should contain the child conclusion: {output}"
    );
    assert!(
        output.contains("status: completed"),
        "should be completed: {output}"
    );
    assert!(
        output.contains("turns: 2"),
        "two steps (Grep + wrap-up): {output}"
    );
    assert!(
        output.contains("resume_hint: continue this subagent with Agent(resume=\""),
        "should carry resume_hint: {output}"
    );
    let agent_id = parse_agent_id(output).to_string();

    // Live rows reuse the final projection ids (`u/r/a/t…`), so a step updates
    // its row in place instead of leaving a transient live-*/input-* duplicate
    // that is later retracted.
    let live_items: Vec<&str> = collected
        .iter()
        .filter_map(|event| match event {
            Event::ConversationUpdate {
                agent_id: Some(id),
                item,
                ..
            } if id == &agent_id => Some(item.id()),
            _ => None,
        })
        .collect();
    assert!(
        !live_items.is_empty(),
        "the child conversation should stream live items"
    );
    assert!(
        live_items.iter().all(|id| {
            id.starts_with('u') || id.starts_with('r') || id.starts_with('a') || id.starts_with('t')
        }),
        "live rows must use final projection ids: {live_items:?}"
    );
    assert!(
        !collected.iter().any(|event| matches!(
            event,
            Event::ConversationRemove { agent_id: Some(id), .. } if id == &agent_id
        )),
        "a clean child run should not retract streamed rows"
    );

    // The parent rollout has a single Agent tool record (child tool calls do not enter the parent rollout)
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl"))).unwrap();
    let tool_records: Vec<&str> = rollout
        .lines()
        .filter(|line| line.contains("\"type\":\"tool_call\""))
        .collect();
    assert_eq!(
        tool_records.len(),
        1,
        "parent rollout should contain only the Agent record: {rollout}"
    );
    assert!(tool_records[0].contains("\"tool\":\"Agent\""));
    assert!(
        !rollout.contains("\"tool\":\"Grep\""),
        "child tools should not enter the parent rollout: {rollout}"
    );

    // Subagent context JSONL uses the root conversation schema: first line
    // meta, then system/user/assistant(Grep)/tool/assistant records.
    let log_path = agent_log_path(&data_dir, &sid, &agent_id);
    let log = std::fs::read_to_string(&log_path).unwrap_or_else(|e| {
        panic!(
            "subagent context should exist at {}: {e}",
            log_path.display()
        )
    });
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines.len() >= 5, "meta + at least 4 records: {log}");
    assert!(
        lines[0].contains("\"type\":\"meta\""),
        "first line is meta: {log}"
    );
    assert!(lines[0].contains(&agent_id));
    assert!(lines[0].contains("\"profile\":\"explore\""));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("\"type\":\"system\""))
    );
    assert!(lines.iter().any(|line| line.contains("\"type\":\"user\"")));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("\"type\":\"tool_call\""))
    );

    // Full result persisted (written in foreground mode too): {agents_dir}/{agent_id}.result.md
    let result_path = log_path.with_file_name(format!("{agent_id}.result.md"));
    let result_text = std::fs::read_to_string(&result_path)
        .unwrap_or_else(|e| panic!("result file should exist at {}: {e}", result_path.display()));
    assert!(
        result_text.contains(mock::SUBAGENT_CHILD_DONE),
        "result file should hold the full child conclusion: {result_text}"
    );

    // Event stream: has SubagentProgress; the child tool (Grep) emits no top-level ToolCallBegin
    let progress: Vec<&str> = collected
        .iter()
        .filter_map(|e| match e {
            Event::SubagentProgress { item_id, note, .. } if item_id.contains("call_agent_1") => {
                Some(note.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(
        progress.iter().any(|note| note.contains("Step 1")),
        "should have step progress: {progress:?}"
    );
    assert!(
        progress.iter().any(|note| note.contains('·')),
        "tool execution should also produce progress lines: {progress:?}"
    );
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "Agent")),
        "parent timeline should have an Agent card"
    );
    assert!(
        collected
            .iter()
            .all(|e| !matches!(e, Event::ToolCallBegin { tool, .. } if tool != "Agent")),
        "child tools should not emit ToolCallBegin: {collected:#?}"
    );

    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).unwrap();
    let mut stmt = db
        .prepare(
            "SELECT step, agent_id, background, provider, model, usage_reported, request_id
             FROM api_usage
             WHERE session_id = ?1 AND source = 'subagent' ORDER BY step",
        )
        .unwrap();
    let rows: Vec<_> = stmt
        .query_map(rusqlite::params![sid], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    assert_eq!(rows.len(), 2, "two child API steps produce two rows");
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.0, index as i64 + 1);
        assert_eq!(row.1, agent_id);
        assert_eq!(row.2, 0, "foreground child");
        assert_eq!(
            (row.3.as_str(), row.4.as_str()),
            ("Mock Provider", "mock-model")
        );
        assert_eq!(row.5, 0, "this mock child does not report usage");
        assert_eq!(row.6.as_deref(), Some(mock::MOCK_REQUEST_ID));
    }
    let runtime_rows: Vec<(String, String, String, String)> = db
        .prepare(
            "SELECT runtime_runs.agent_id, runtime_runs.status, runtime_agents.profile,
                    runtime_agents.conversation_path
             FROM runtime_runs
             JOIN runtime_agents USING (session_id, agent_id)
             WHERE runtime_runs.session_id = ?1
             ORDER BY runtime_runs.started_at_ms, runtime_runs.run_id",
        )
        .unwrap()
        .query_map(rusqlite::params![sid], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    assert_eq!(
        runtime_rows.len(),
        2,
        "root and child both use AgentRuntime"
    );
    assert!(
        runtime_rows.iter().all(|row| row.1 == "completed"),
        "runtime terminal rows: {runtime_rows:?}"
    );
    let child = runtime_rows
        .iter()
        .find(|row| row.0 == agent_id)
        .expect("child runtime run");
    assert_eq!(child.2, "explore");
    assert_eq!(
        std::fs::canonicalize(&child.3).unwrap(),
        std::fs::canonicalize(&log_path).unwrap()
    );
    let root = runtime_rows
        .iter()
        .find(|row| row.0 == sid)
        .expect("root runtime run");
    assert_eq!(root.2, "main");
    assert_eq!(
        std::fs::canonicalize(&root.3).unwrap(),
        std::fs::canonicalize(data_dir.join("sessions").join(format!("{sid}.jsonl")),).unwrap()
    );
    agent.shutdown();
}

/// Foreground single Agent: a child's Write is part of the parent turn's
/// TurnChanges (one net entry) and the persisted original is the earliest
/// state (created, NULL), not the child's output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_foreground_write_enters_turn_changes_and_original() {
    let (config_path, cwd, data_dir) = setup("subagent-turn-changes");
    let path = cwd.join("child_write.txt");
    assert!(!path.exists());
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd.clone()).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} WRITE general-purpose", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&collected).expect("Agent ToolCallEnd");
    assert!(!is_error, "{output}");

    let files = turn_file_changes(&collected);
    assert_eq!(
        files.len(),
        1,
        "one net entry for the child write: {files:?}"
    );
    assert_eq!(files[0].path, "child_write.txt");
    assert!(
        files[0].unified_diff.contains("+x"),
        "the child write is in the parent turn summary: {}",
        files[0].unified_diff
    );
    assert_eq!(
        file_originals(&data_dir, &sid, "child_write.txt"),
        vec![None],
        "the persisted original is the earliest state (created), not the child's output"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\n");

    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl"))).unwrap();
    let records: Vec<serde_json::Value> = rollout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record| record["type"] == "turn_changes")
        .collect();
    assert_eq!(
        records.len(),
        1,
        "one durable TurnChanges record: {rollout}"
    );
    assert_eq!(
        records[0]["files"].as_array().map(Vec::len),
        Some(1),
        "the durable record carries one file: {records:?}"
    );
    let durable_diff = records[0]["files"][0]["unified_diff"]
        .as_str()
        .expect("durable unified diff");
    assert_eq!(
        durable_diff, files[0].unified_diff,
        "the durable TurnChanges content matches the live event"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_uses_default_tool_choice() {
    let (config_path, cwd, data_dir, log) = setup_with_log("subagent-step-choice");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str("\nparallel_tool_calls = false\n");
    std::fs::write(&config_path, config).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&collected).expect("Agent result");
    assert!(!is_error, "{output}");
    assert!(output.contains("turns: 2"), "{output}");

    let bodies: Vec<serde_json::Value> = log
        .lock()
        .expect("request log")
        .iter()
        .filter_map(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(|body| {
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "user"
                        && message["content"]
                            .as_str()
                            .is_some_and(|text| text.starts_with("SUBAGENT_CHILD:"))
                })
            })
        })
        .collect();
    assert_eq!(bodies.len(), 2, "{bodies:#?}");
    for body in &bodies {
        assert!(body.get("tool_choice").is_none(), "{body}");
        assert_eq!(body["parallel_tool_calls"], serde_json::json!(false));
    }
    agent.shutdown();
}

/// Tool narrowing: the explore subagent calls Write (not in its tool set) →
/// can still wrap up after receiving the "unknown tool" error, the file does
/// not land, ultimately completed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_tool_narrowing_blocks_write() {
    let (config_path, cwd, data_dir) = setup("subagent-narrow");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd.clone()).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} WRITE", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;

    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(
        !is_error,
        "subagent should still wrap up after an unknown tool: {output}"
    );
    assert!(
        output.contains(mock::SUBAGENT_CHILD_DONE),
        "should contain the child conclusion: {output}"
    );
    assert!(output.contains("status: completed"), "{output}");
    assert!(
        !cwd.join("child_write.txt").exists(),
        "the narrowed-out Write should not execute"
    );
    agent.shutdown();
}

/// Approval gate: under ReadOnly the general-purpose subagent's Write pops an
/// approval; after approval the file is created. (Shell commands run without a
/// routine prompt in ReadOnly; the read-only sandbox is their gate.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_child_tools_pass_approval_gate() {
    let (config_path, cwd, data_dir) = setup("subagent-approval");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd.clone()).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} WRITE general-purpose", mock::SUBAGENT_TRIGGER),
        ExecMode::ReadOnly,
        Some(ApprovalDecision::Allow),
    )
    .await;

    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ApprovalRequested { tool, .. } if tool == "Write")),
        "subagent Write should prompt for approval: {collected:#?}"
    );
    assert!(
        cwd.join("child_write.txt").exists(),
        "after approval the write actually executes (file created)"
    );
    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(!is_error, "{output}");
    assert!(output.contains("status: completed"), "{output}");
    agent.shutdown();
}

/// Project profiles are repository content: without a trust decision the
/// Agent tool must not activate them (the same token that admits hooks/MCP).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_project_profile_requires_trust() {
    let (config_path, cwd, data_dir) = setup("subagent-project-profile");
    std::fs::create_dir_all(cwd.join(".pigcode/agents")).unwrap();
    std::fs::write(
        cwd.join(".pigcode/agents/denied.md"),
        "---\nname: denied\ndescription: untrusted project profile\n---\nProject body.",
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP denied", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&collected).expect("Agent ToolCallEnd");
    assert!(
        is_error,
        "an untrusted project profile must not run: {output}"
    );
    assert!(output.contains("No subagent found"), "{output}");
    assert!(
        output.contains("denied"),
        "the refusal names the requested profile: {output}"
    );
    agent.shutdown();
}

/// The trusted project provider/model layer is one runtime object for root and
/// child resolution. The test starts untrusted (root on the user model), then
/// uses the live trust-refresh op: the next root turn and a project profile
/// pinned to the project model (bare id, so it resolves through the project's
/// default provider) must both run on it, and the child metadata records the
/// project provider name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trusted_project_provider_model_is_shared_by_root_and_child() {
    const CHILD_BODY_MARKER: &str = "PROJECT_CHILD_PROFILE_BODY_7D1";
    let (port, log) = mock::start_mock_server_with_log();
    let root = std::env::temp_dir().join(format!(
        "pig-core-subagent-project-runtime-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let cwd = root.join("ws");
    let data_dir = root.join("data");
    std::fs::create_dir_all(cwd.join(".pigcode/agents")).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(cwd.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    std::fs::write(
        cwd.join(".pigcode/agents/proj-model.md"),
        format!(
            "---\nname: proj-model\ndescription: PROJECT_MODEL_CHILD_PROFILE\n\
             tools: [Read, Grep]\nmodel: project-model\n---\n\n{CHILD_BODY_MARKER}\n"
        ),
    )
    .unwrap();
    let provider = |id: &str, name: &str, model: &str| {
        format!(
            "[[providers]]\nid = \"{id}\"\nname = \"{name}\"\n\
             base_url = \"http://127.0.0.1:{port}/v1\"\napi_key = \"mock-key\"\n\
             api_format = \"OpenAiChat\"\nenabled = true\n\
             [[providers.models]]\nid = \"{model}\"\ncontext_window = 128000\n\
             max_output_tokens = 8192\n"
        )
    };
    // A project config may select an existing user provider/model, never
    // inject a provider endpoint or credential.
    std::fs::write(
        cwd.join(".pigcode/config.toml"),
        "default_provider = \"proj\"\ndefault_model = \"project-model\"\n",
    )
    .unwrap();
    let config_path = root.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "default_provider = \"user\"\ndefault_model = \"user-model\"\n{}{}",
            provider("user", "User", "user-model"),
            provider("proj", "Project", "project-model")
        ),
    )
    .unwrap();

    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd.clone()).await;

    // Untrusted: the project layer is absent and the root runs on the user
    // model.
    let first = run_turn(&agent, &sid, "before trust", ExecMode::Auto, None).await;
    assert!(
        first
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the pre-trust turn completes"
    );
    let conversation = |log: &mock::RequestLog| -> Vec<serde_json::Value> {
        log.lock()
            .unwrap()
            .iter()
            .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
            .filter(|request| {
                request["messages"][0]["role"] == "system"
                    && request["messages"][0]["content"]
                        .as_str()
                        .is_some_and(|content| content.contains("Pig Code"))
                    && request["tools"]
                        .as_array()
                        .is_some_and(|tools| !tools.is_empty())
            })
            .collect()
    };
    let pre_trust = conversation(&log);
    assert_eq!(
        pre_trust
            .last()
            .and_then(|request| request["model"].as_str()),
        Some("user-model"),
        "untrusted root resolution stays on the user model: {pre_trust:#?}"
    );

    // Live trust refresh: no new session, the stored runtime is replaced.
    agent
        .ops
        .send(Op::TrustProjectHooks {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut trusted = false;
    while !trusted {
        assert!(
            Instant::now() < deadline,
            "ProjectHooksTrusted was not emitted"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        trusted = matches!(event, Event::ProjectHooksTrusted { .. });
    }

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP proj-model", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&collected).expect("Agent ToolCallEnd");
    assert!(!is_error, "project child should run: {output}");
    let agent_id = parse_agent_id(output).to_string();

    let requests = conversation(&log);
    let root_request = requests
        .iter()
        .rev()
        .find(|request| request.to_string().contains(mock::SUBAGENT_TRIGGER))
        .expect("trusted root request");
    assert_eq!(
        root_request["model"].as_str(),
        Some("project-model"),
        "the live trust refresh applies the project root model: {root_request:#?}"
    );
    let child_request = log
        .lock()
        .unwrap()
        .iter()
        .rev()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .find(|request| request.to_string().contains(CHILD_BODY_MARKER))
        .expect("project child request");
    assert_eq!(
        child_request["model"].as_str(),
        Some("project-model"),
        "the child resolves the project-pinned model from the same runtime: {child_request:#?}"
    );
    assert_eq!(
        root_request["model"].as_str(),
        child_request["model"].as_str(),
        "root and child see the identical project model"
    );

    // Provider identity is recorded in the child metadata, proving the pin
    // resolved through the trusted project provider, not the user provider.
    let meta_line = std::fs::read_to_string(agent_log_path(&data_dir, &sid, &agent_id))
        .expect("child context")
        .lines()
        .next()
        .expect("meta line")
        .to_string();
    let meta: serde_json::Value = serde_json::from_str(&meta_line).expect("meta json");
    assert_eq!(meta["model"].as_str(), Some("project-model"));
    assert_eq!(meta["provider"].as_str(), Some("Project"));
    agent.shutdown();
}

/// max_turns: user-level profile maxTurns: 2 + the subagent returning a tool_call every step → wraps up when turns run out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_max_turns_exhausted() {
    let (config_path, cwd, data_dir) = setup("subagent-maxturns");
    std::fs::create_dir_all(data_dir.join("agents")).unwrap();
    std::fs::write(
        data_dir.join("agents/loop.md"),
        "---\nname: loop\ndescription: loop test subagent\nmaxTurns: 2\n---\nLoop profile body.",
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} LOOP loop", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;

    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(
        is_error,
        "exhausted turns with no conclusion should be is_error: {output}"
    );
    assert!(
        output.contains("Reached the maximum turn count (2)"),
        "{output}"
    );
    assert!(
        output.contains("agent_id: ") && output.contains("status: failed"),
        "max-turn receipts keep the agent identity and terminal status: {output}"
    );
    assert!(
        output.contains("resume_hint: continue this subagent with Agent(resume=\""),
        "max-turn receipts keep resume guidance: {output}"
    );
    agent.shutdown();
}

/// Strict model resolution failure: the profile's model points to a nonexistent provider → the result is is_error and lists available models.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_model_resolve_failure_is_strict() {
    let (config_path, cwd, data_dir) = setup("subagent-badmodel");
    std::fs::create_dir_all(data_dir.join("agents")).unwrap();
    std::fs::write(
        data_dir.join("agents/badmodel.md"),
        "---\nname: badmodel\ndescription: bad-model test subagent\nmodel: p9/m9\n---\nBad-model profile body.",
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP badmodel", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;

    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(
        is_error,
        "model resolve failure should be is_error: {output}"
    );
    assert!(
        output.contains("p9"),
        "should name the bad provider: {output}"
    );
    assert!(
        output.contains("mock/mock-model"),
        "should list available providerId/modelId: {output}"
    );
    agent.shutdown();
}

/// Result truncation: subagent final text >32K chars → the Agent result is truncated and points to the persisted full-text file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_result_truncated_and_spilled() {
    let (config_path, cwd, data_dir) = setup("subagent-truncate");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd.clone()).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} LONG", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;

    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(!is_error, "truncation is not an error: {output}");
    assert!(output.contains("status: completed"), "{output}");
    assert!(
        output.contains("[Result too long; truncated. Full text: "),
        "should contain the truncation pointer: {output}"
    );
    // Full text persisted: {data}/sessions/{sid}.agents/{agent_id}.result.md
    // (kimi's output.log equivalent), content complete and unmodified; the
    // truncation pointer references it
    let agent_id = parse_agent_id(output);
    assert!(
        output.contains(".result.md"),
        "truncation pointer should reference result.md: {output}"
    );
    let spill =
        agent_log_path(&data_dir, &sid, agent_id).with_file_name(format!("{agent_id}.result.md"));
    let full = std::fs::read_to_string(&spill)
        .unwrap_or_else(|e| panic!("full text should be spilled to {}: {e}", spill.display()));
    let expected = format!("Subagent long result start. {}", "密".repeat(33_000));
    assert_eq!(
        full, expected,
        "spilled file should be the untruncated full text"
    );
    agent.shutdown();
}

/// Plan mode rejection: delegating a subagent with plan mode on → "cannot delegate in plan mode".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_plan_mode_rejected() {
    let (config_path, cwd, data_dir) = setup("subagent-plan");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SetPlanMode {
            session_id: sid.clone(),
            enabled: true,
        })
        .await
        .unwrap();
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;

    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(is_error, "plan-mode delegation should fail: {output}");
    assert!(
        output.contains("Subagents cannot be delegated in plan mode"),
        "{output}"
    );
    agent.shutdown();
}

/// (output, is_error) of the Agent tool card by call id
fn agent_end_with<'a>(events: &'a [Event], call_id: &str) -> Option<(&'a str, bool)> {
    events.iter().find_map(|e| match e {
        Event::ToolCallEnd {
            item_id,
            output,
            is_error,
            ..
        } if item_id.contains(call_id) => Some((output.as_str(), *is_error)),
        _ => None,
    })
}

/// msg line count of the subagent context JSONL
fn agent_log_msg_count(data_dir: &Path, sid: &str, agent_id: &str) -> usize {
    let log = std::fs::read_to_string(agent_log_path(data_dir, sid, agent_id)).unwrap();
    log.lines()
        .filter(|line| !line.contains("\"type\":\"meta\""))
        .count()
}

/// Background full link: run_in_background=true → the parent immediately
/// receives running+task_id; after the parent turn ends we wait for the
/// synthetic <task-notification> user message and the TurnComplete after it;
/// the task panel contains a subagent entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_background_full_link() {
    let (config_path, cwd, data_dir) = setup("subagent-bg-full");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} BG", mock::SUBAGENT_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut running_output: Option<String> = None;
    let mut notification: Option<pig_protocol::TaskNotification> = None;
    let mut completes = 0usize;
    let mut saw_agent_task = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the background full link"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::ToolCallEnd {
                item_id,
                output,
                is_error,
                ..
            } if item_id.contains("call_agent_bg") => {
                assert!(!is_error, "background delegation should not fail: {output}");
                running_output = Some(output.clone());
            }
            Event::TaskNotificationBatch { notifications, .. } => {
                notification = notifications.first().cloned();
            }
            Event::TaskListChanged { tasks, .. } => {
                saw_agent_task |= tasks.iter().any(|t| t.command.contains("Subagent"));
            }
            Event::TurnComplete { .. } => completes += 1,
            _ => {}
        }
        if completes >= 2 && notification.is_some() {
            break;
        }
    }

    // The parent immediately receives the running receipt (task_id queryable)
    let running = running_output.expect("should have background Agent ToolCallEnd");
    assert!(running.contains("status: running"), "{running}");
    assert!(running.contains("task_id: b"), "{running}");
    let agent_id = parse_agent_id(&running).to_string();

    let notification = notification.expect("should have a structured completion notification");
    assert_eq!(notification.kind, pig_protocol::TaskKind::Subagent);
    assert_eq!(notification.status, pig_protocol::TaskStatus::Completed);
    assert_eq!(notification.agent_id.as_deref(), Some(agent_id.as_str()));
    assert_eq!(notification.turns, Some(2));
    assert!(notification.duration_ms.is_some());
    assert!(
        notification
            .description
            .contains("subagent selftest delegation")
    );
    let expected_record = dunce::canonicalize(agent_log_path(&data_dir, &sid, &agent_id))
        .expect("canonical record path");
    assert_eq!(
        notification
            .record_path
            .as_ref()
            .map(|path| dunce::canonicalize(path).expect("canonical record path")),
        Some(expected_record)
    );
    assert!(
        !notification.body.contains(mock::SUBAGENT_CHILD_DONE),
        "the body must not inline the full conclusion: {}",
        notification.body
    );
    let result_path =
        agent_log_path(&data_dir, &sid, &agent_id).with_file_name(format!("{agent_id}.result.md"));
    let Some(pig_protocol::TaskNotificationOutput::File { path, bytes }) = &notification.output
    else {
        panic!("the notification should point to the result file")
    };
    assert_eq!(
        dunce::canonicalize(path).expect("canonical result path"),
        dunce::canonicalize(&result_path).expect("canonical result path")
    );
    assert!(*bytes > 0);
    let full = std::fs::read_to_string(&result_path)
        .unwrap_or_else(|e| panic!("result file should exist at {}: {e}", result_path.display()));
    assert!(
        full.contains(mock::SUBAGENT_CHILD_DONE),
        "result file should hold the full child conclusion: {full}"
    );
    assert!(saw_agent_task, "task panel should contain a subagent entry");

    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).unwrap();
    let mut stmt = db
        .prepare(
            "SELECT agent_id, background, usage_reported FROM api_usage
             WHERE session_id = ?1 AND source = 'subagent' ORDER BY step",
        )
        .unwrap();
    let rows: Vec<_> = stmt
        .query_map(rusqlite::params![sid], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    assert_eq!(rows.len(), 2, "background child steps are persisted too");
    for row in rows {
        assert_eq!(row.0, agent_id);
        assert_eq!(row.1, 1, "background child");
        assert_eq!(row.2, 0, "this mock child does not report usage");
    }
    agent
        .ops
        .send(Op::AgentOutput {
            session_id: sid.clone(),
            agent_id: agent_id.clone(),
        })
        .await
        .unwrap();
    let output = crate::common::recv_until(&agent.events, Duration::from_secs(5), |event| {
        matches!(event, Event::AgentOutput { .. })
    })
    .await;
    let Some(Event::AgentOutput { run_id, text, .. }) = output.last() else {
        panic!("expected partial agent output")
    };
    assert!(run_id.is_some());
    assert!(text.contains(mock::SUBAGENT_CHILD_DONE), "{text}");
    agent.shutdown();
}

/// resume continuation: run once in the foreground, then resume (new prompt) →
/// the jsonl grows, the result contains the new conclusion, and the
/// resume_hint's agent_id stays unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_continues_context() {
    let (config_path, cwd, data_dir) = setup("subagent-resume");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output1, is_error1) = agent_end(&collected).expect("first Agent ToolCallEnd");
    assert!(!is_error1, "{output1}");
    let agent_id = parse_agent_id(output1).to_string();
    let msgs_before = agent_log_msg_count(&data_dir, &sid, &agent_id);

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output2, is_error2) =
        agent_end_with(&collected, "call_agent_2").expect("resume Agent ToolCallEnd");
    assert!(!is_error2, "resume should succeed: {output2}");
    assert!(
        output2.contains(mock::SUBAGENT_RESUMED_DONE),
        "should contain the new resumed conclusion: {output2}"
    );
    assert!(output2.contains("status: completed"), "{output2}");
    assert_eq!(
        parse_agent_id(output2),
        agent_id,
        "resume_hint agent_id should stay unchanged"
    );
    let msgs_after = agent_log_msg_count(&data_dir, &sid, &agent_id);
    assert!(
        msgs_after >= msgs_before + 2,
        "resume should append >=2 messages (new prompt + new conclusion): {msgs_before} -> {msgs_after}"
    );
    agent.shutdown();
}

/// A resume whose new prompt cannot be appended fails the Agent call with the
/// agent_id and resume guidance instead of running a live child the JSONL does
/// not contain. The writer claim is released, so fixing the disk and retrying
/// the resume succeeds.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_write_failure_fails_with_guidance_and_releases_writer() {
    use std::os::unix::fs::PermissionsExt as _;

    let (config_path, cwd, data_dir) = setup("subagent-resume-write-fail");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    let trigger = format!("{} RESUME", mock::SUBAGENT_TRIGGER);

    let first = run_turn(&agent, &sid, &trigger, ExecMode::Auto, None).await;
    let (out1, err1) = agent_end(&first).expect("first Agent ToolCallEnd");
    assert!(!err1, "{out1}");
    let agent_id = parse_agent_id(out1).to_string();
    let jsonl = agent_log_path(&data_dir, &sid, &agent_id);
    assert!(jsonl.is_file());

    let original = std::fs::metadata(&jsonl)
        .expect("context metadata")
        .permissions();
    std::fs::set_permissions(&jsonl, std::fs::Permissions::from_mode(0o444))
        .expect("make the context read-only");

    let second = run_turn(&agent, &sid, &trigger, ExecMode::Auto, None).await;
    let (out2, err2) =
        agent_end_with(&second, "call_agent_2").expect("failed resume Agent ToolCallEnd");
    assert!(err2, "a context write failure is a tool failure: {out2}");
    assert!(
        out2.contains("could not persist its context"),
        "the failure names persistence: {out2}"
    );
    assert!(
        out2.contains(&format!("Subagent {agent_id}")),
        "the failure carries the agent_id: {out2}"
    );
    assert!(
        out2.contains(&format!("Agent(resume=\"{agent_id}\"")),
        "the failure carries resume guidance: {out2}"
    );

    // The writer claim was released on the failure path: after fixing the
    // file, the same resume succeeds instead of reporting a still-closing
    // driver.
    std::fs::set_permissions(&jsonl, original).expect("restore context permissions");
    let third = run_turn(&agent, &sid, &trigger, ExecMode::Auto, None).await;
    let (out3, err3) =
        agent_end_with(&third, "call_agent_3").expect("retried resume Agent ToolCallEnd");
    assert!(
        !err3,
        "the writer was released after the failed append: {out3}"
    );
    assert!(out3.contains("status: completed"), "{out3}");
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_preserves_child_request_prefix() {
    let (config_path, cwd, data_dir, log) = common::setup_with_log("subagent-resume-prefix");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    let child_requests = || {
        log.lock()
            .expect("request log")
            .iter()
            .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
            .filter(|request| {
                request["stream"] == true
                    && request.to_string().contains("SUBAGENT_CHILD:")
                    && request["messages"][0]["content"]
                        .as_str()
                        .is_some_and(|system| !system.contains("Pig Code"))
            })
            .collect::<Vec<_>>()
    };
    run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let before = child_requests()
        .last()
        .expect("first child request")
        .clone();
    run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let after = child_requests()
        .last()
        .expect("resumed child request")
        .clone();
    assert!(after.to_string().contains("RESUMED"));
    let before_messages = before["messages"].as_array().expect("before messages");
    let after_messages = after["messages"].as_array().expect("after messages");
    assert!(after_messages.len() > before_messages.len());
    for (index, message) in before_messages.iter().enumerate() {
        assert_eq!(
            message, &after_messages[index],
            "child message {index} changed"
        );
    }
    agent.shutdown();
}

/// A child model without PDF input must receive the resumed history with the
/// document removed and a deterministic note, not a provider-preflight failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_projects_pdf_out_when_child_lacks_pdf_input() {
    let (config_path, cwd, data_dir, log) = common::setup_with_log("subagent-resume-pdf-off");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    seed_pdf_agent(&data_dir, &sid, "a9-1", &pdf_fixture());

    run_turn(
        &agent,
        &sid,
        &format!("{} GREP agent_id: a9-1", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) =
        agent_end_with(&collected, "call_agent_2").expect("resume Agent ToolCallEnd");
    assert!(
        !is_error,
        "resume must not hit provider preflight: {output}"
    );
    assert!(output.contains(mock::SUBAGENT_RESUMED_DONE), "{output}");

    let requests = resumed_child_requests(&log);
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let request = requests[0].to_string();
    assert!(
        !request.contains("application/pdf") && !request.contains("file_data"),
        "the unsupported document never reaches the wire: {request}"
    );
    assert!(
        request.contains("PDF document(s) were not sent")
            && request.contains("does not support PDF input"),
        "the projection adds the deterministic note: {request}"
    );
    // Projection is in-memory only: the durable ref and media file remain for
    // a later PDF-capable resume.
    let raw = std::fs::read_to_string(agent_log_path(&data_dir, &sid, "a9-1")).unwrap();
    assert!(raw.contains("\"documents\""), "{raw}");
    assert!(
        agent_log_path(&data_dir, &sid, "a9-1")
            .with_extension("media")
            .join("1.pdf")
            .is_file(),
        "the durable document ref stays usable"
    );
    agent.shutdown();
}

/// A missing persisted PDF degrades to the root replay's note on resume
/// instead of silently dropping the tool result's attachment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_missing_pdf_degrades_with_note() {
    let (config_path, cwd, data_dir, log) = common::setup_with_log("subagent-resume-pdf-missing");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    let media = seed_pdf_agent(&data_dir, &sid, "a9-1", &pdf_fixture());
    std::fs::remove_file(&media).unwrap();

    run_turn(
        &agent,
        &sid,
        &format!("{} GREP agent_id: a9-1", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) =
        agent_end_with(&collected, "call_agent_2").expect("resume Agent ToolCallEnd");
    assert!(!is_error, "{output}");
    assert!(output.contains(mock::SUBAGENT_RESUMED_DONE), "{output}");
    let requests = resumed_child_requests(&log);
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let request = requests[0].to_string();
    assert!(
        !request.contains("application/pdf") && !request.contains("file_data"),
        "a missing document is never sent: {request}"
    );
    assert!(
        request.contains("[1 PDF document(s) no longer available]"),
        "the root replay's deterministic missing note is present: {request}"
    );
    agent.shutdown();
}

/// Resuming under an image-incapable child removes rehydrated images with a
/// deterministic note, and the durable JSONL/media refs remain untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_projects_images_out_when_child_lacks_image_input() {
    let (config_path, cwd, data_dir, log) = common::setup_with_log("subagent-resume-image-off");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    let media = seed_image_agent(&data_dir, &sid, "a9-1");

    run_turn(
        &agent,
        &sid,
        &format!("{} GREP agent_id: a9-1", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) =
        agent_end_with(&collected, "call_agent_2").expect("resume Agent ToolCallEnd");
    assert!(!is_error, "{output}");
    assert!(output.contains(mock::SUBAGENT_RESUMED_DONE), "{output}");
    let requests = resumed_child_requests(&log);
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let request = requests[0].to_string();
    assert!(
        !request.contains("data:image") && !request.contains("\"type\":\"image\""),
        "the unsupported image never reaches the wire: {request}"
    );
    assert!(
        request.contains("image(s) were not sent")
            && request.contains("does not support image input"),
        "the projection adds the deterministic image note: {request}"
    );
    let raw = std::fs::read_to_string(agent_log_path(&data_dir, &sid, "a9-1")).unwrap();
    assert!(raw.contains("\"images\""), "{raw}");
    assert!(media.is_file(), "the durable image ref stays usable");
    agent.shutdown();
}

/// Child replay validates persisted image bytes: a corrupt file degrades to
/// the deterministic missing note (never broken bytes to the provider), even
/// when the child model accepts images.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_corrupt_image_degrades_with_note() {
    let (config_path, cwd, data_dir, log) = common::setup_with_log("subagent-resume-image-corrupt");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str("\ninput_image = true\n");
    std::fs::write(&config_path, config).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    let media = seed_image_agent(&data_dir, &sid, "a9-1");
    std::fs::write(&media, b"not an image").unwrap();

    run_turn(
        &agent,
        &sid,
        &format!("{} GREP agent_id: a9-1", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) =
        agent_end_with(&collected, "call_agent_2").expect("resume Agent ToolCallEnd");
    assert!(!is_error, "{output}");
    assert!(output.contains(mock::SUBAGENT_RESUMED_DONE), "{output}");
    let requests = resumed_child_requests(&log);
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let request = requests[0].to_string();
    assert!(
        !request.contains("data:image") && !request.contains("\"type\":\"image\""),
        "a corrupt image is never sent: {request}"
    );
    assert!(
        request.contains("[1 image(s) no longer available]"),
        "the deterministic corrupt-media note is present: {request}"
    );
    // The durable ref and corrupt bytes stay untouched: a later repair/resume
    // can rehydrate the same ref.
    assert!(media.is_file());
    let raw = std::fs::read_to_string(agent_log_path(&data_dir, &sid, "a9-1")).unwrap();
    assert!(raw.contains("\"images\""), "{raw}");
    agent.shutdown();
}

/// A real PDF on disk is rehydrated into the resumed child request when the
/// child model supports PDF input (the projection is capability-driven).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_rehydrates_real_pdf_when_supported() {
    let (config_path, cwd, data_dir, log) = common::setup_with_log("subagent-resume-pdf-on");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str("\ninput_pdf = true\n");
    std::fs::write(&config_path, config).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    seed_pdf_agent(&data_dir, &sid, "a9-1", &pdf_fixture());

    run_turn(
        &agent,
        &sid,
        &format!("{} GREP agent_id: a9-1", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) =
        agent_end_with(&collected, "call_agent_2").expect("resume Agent ToolCallEnd");
    assert!(!is_error, "{output}");
    assert!(output.contains(mock::SUBAGENT_RESUMED_DONE), "{output}");
    let requests = resumed_child_requests(&log);
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let request = requests[0].to_string();
    assert!(
        request.contains("\"file_data\":\"data:application/pdf;base64,"),
        "the supported document is rehydrated into the wire request: {request}"
    );
    agent.shutdown();
}

/// resume with unknown id: errors and lists available ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_unknown_id_lists_available() {
    let (config_path, cwd, data_dir) = setup("subagent-resume-unknown");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    // Run once first to obtain a real agent_id
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output1, _) = agent_end(&collected).expect("first Agent ToolCallEnd");
    let real_id = parse_agent_id(output1).to_string();

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME_UNKNOWN", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output2, is_error2) =
        agent_end_with(&collected, "call_agent_2").expect("resume Agent ToolCallEnd");
    assert!(is_error2, "unknown id should fail: {output2}");
    assert!(
        output2.contains("a0-999"),
        "should name the unknown id: {output2}"
    );
    assert!(output2.contains("does not exist"), "{output2}");
    assert!(
        output2.contains(&real_id),
        "should list available agent_id: {output2}"
    );
    agent.shutdown();
}

/// resume conflict while running: resuming while the background subagent is still running → reports "still running".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_running_conflict() {
    let (config_path, cwd, data_dir) = setup("subagent-resume-running");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    // First message: a background LOOP subagent (runs forever)
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME_RUNNING", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output1, is_error1) =
        agent_end_with(&collected, "call_agent_bg").expect("background Agent ToolCallEnd");
    assert!(!is_error1, "{output1}");
    assert!(output1.contains("status: running"), "{output1}");

    // Second message: immediately resume the same id → conflict
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME_RUNNING", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output2, is_error2) =
        agent_end_with(&collected, "call_agent_2").expect("resume Agent ToolCallEnd");
    assert!(is_error2, "resume while running should fail: {output2}");
    assert!(output2.contains("still running"), "{output2}");
    agent.shutdown();
}

/// TaskStop produces a Killed terminal notification; the parent cannot assume
/// the cancelled child is still running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_background_taskstop_notifies_killed() {
    let (config_path, cwd, data_dir) = setup("subagent-bg-stop");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let mut collected = run_turn(
        &agent,
        &sid,
        &format!("{} BGSTOP", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (stop_output, stop_error) =
        agent_end_with(&collected, "call_taskstop").expect("should have TaskStop ToolCallEnd");
    assert!(!stop_error, "TaskStop should succeed: {stop_output}");

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), agent.events.recv()).await
        else {
            continue;
        };
        collected.push(event);
    }
    let killed = collected.iter().any(|e| {
        matches!(e, Event::TaskListChanged { tasks, .. } if tasks
            .iter()
            .any(|t| t.command.contains("Subagent") && t.status == pig_protocol::TaskStatus::Killed))
    });
    assert!(killed, "registry should have a Killed subagent task");
    assert!(
        collected.iter().any(|event| matches!(
            event,
            Event::TaskNotificationBatch { notifications, .. }
                if notifications.iter().any(|notification| notification.status == pig_protocol::TaskStatus::Killed)
        )),
        "TaskStop must deliver the Killed terminal notification: {collected:#?}"
    );
    agent.shutdown();
}

/// Stop → resume: after TaskStop, the driver must terminate and release the
/// JSONL writer before the next turn resumes the same agent. The terminal
/// ConversationStatus event is the closing signal; resume then succeeds on the
/// same context instead of being rejected or appending concurrently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_stop_then_resume_succeeds_after_driver_settles() {
    let (config_path, cwd, data_dir) = setup("subagent-stop-resume");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} BGSTOP", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (running, running_error) =
        agent_end_with(&collected, "call_agent_bg").expect("background Agent ToolCallEnd");
    assert!(!running_error, "{running}");
    let agent_id = parse_agent_id(running).to_string();

    // Wait for the stopped driver's terminal panel event: this is the closing
    // signal that releases the exclusive context writer.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut settled = collected.iter().any(|event| {
        matches!(
            event,
            Event::ConversationStatus {
                agent_id: settled_id,
                running: false,
                ..
            } if settled_id == &agent_id
        )
    });
    while Instant::now() < deadline && !settled {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), agent.events.recv()).await
        else {
            continue;
        };
        if matches!(
            &event,
            Event::ConversationStatus {
                agent_id: settled_id,
                running: false,
                ..
            } if settled_id == &agent_id
        ) {
            settled = true;
        }
    }
    assert!(settled, "the stopped driver did not settle");

    // The next turn resumes the same agent_id; by now the writer is free.
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (resumed, resumed_error) = agent_end_with(&collected, "call_agent_2")
        .unwrap_or_else(|| panic!("resume Agent ToolCallEnd: {collected:#?}"));
    assert!(
        !resumed_error,
        "resume after a settled stop should succeed: {resumed}"
    );
    assert!(
        resumed.contains(mock::SUBAGENT_RESUMED_DONE),
        "the original context should continue: {resumed}"
    );
    assert!(
        resumed.contains(&format!("agent_id: {agent_id}")),
        "resume reuses the original agent_id: {resumed}"
    );
    agent.shutdown();
}

/// Background approval: under ReadOnly a background subagent's write still
/// goes through the pending approval gate — ApprovalRequested appears →
/// approve → the write executes → the completion notification arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_background_approval_gate() {
    let (config_path, cwd, data_dir) = setup("subagent-bg-approval");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} WRITEBG", mock::SUBAGENT_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut saw_approval = false;
    let mut notification: Option<pig_protocol::TaskNotification> = None;
    let mut bg_output: Option<String> = None;
    let mut completes = 0usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the background approval link"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::ApprovalRequested {
                request_id, tool, ..
            } => {
                assert_eq!(
                    tool, "Write",
                    "background subagent Write should prompt for approval"
                );
                saw_approval = true;
                agent
                    .ops
                    .send(Op::ApprovalReply {
                        request_id: request_id.clone(),
                        decision: ApprovalDecision::Allow,
                        feedback: None,
                    })
                    .await
                    .unwrap();
            }
            Event::TaskNotificationBatch { notifications, .. } => {
                notification = notifications.first().cloned();
            }
            Event::ToolCallEnd {
                item_id, output, ..
            } if item_id.contains("call_agent_bg") => {
                bg_output = Some(output.clone());
            }
            Event::TurnComplete { .. } => completes += 1,
            _ => {}
        }
        if completes >= 2 && notification.is_some() {
            break;
        }
    }

    assert!(
        saw_approval,
        "background subagent should trigger the approval gate"
    );
    assert!(
        cwd.join("child_write.txt").exists(),
        "after approval the background write actually executes (file created)"
    );
    let notification = notification.expect("should have a completion notification");
    assert!(
        !notification.body.contains(mock::SUBAGENT_CHILD_DONE),
        "notification body should not inline the full conclusion: {}",
        notification.body
    );
    assert!(matches!(
        notification.output,
        Some(pig_protocol::TaskNotificationOutput::File { .. })
    ));
    let bg_output = bg_output.expect("should have background Agent ToolCallEnd");
    let agent_id = parse_agent_id(&bg_output);
    let result_path =
        agent_log_path(&data_dir, &sid, agent_id).with_file_name(format!("{agent_id}.result.md"));
    let full = std::fs::read_to_string(&result_path).expect("result file should exist");
    assert!(
        full.contains(mock::SUBAGENT_CHILD_DONE),
        "result file should contain the child conclusion: {full}"
    );
    agent.shutdown();
}

/// Background swarm (run_in_background): the immediate receipt lists
/// agent_id/task_id/status per item + a "do not poll" hint; two live
/// SubagentCards (background=true); each subagent wakes the parent session on
/// completion (two <task-notification> messages); the rollout's ToolCall
/// record carries agent_cards (2 cards); a simulated restart replay rebuilds
/// both cards (same item_id) and appends finished to each to settle the
/// terminal state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_background_receipt_and_cards_replayed() {
    let (config_path, cwd, data_dir) = setup("subagent-swarm-bg");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let sid = new_session(&agent, cwd.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} SWARMBG", mock::SUBAGENT_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    // Terminal items may share a batch; count notifications, not wake turns.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut terminal_count = 0usize;
    let mut swarm_output: Option<String> = None;
    let mut live_cards: Vec<(String, String, bool)> = Vec::new();
    while terminal_count < 2 {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for two completion notifications"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::TaskNotificationBatch { notifications, .. } => {
                terminal_count += notifications.len();
            }
            Event::ToolCallEnd {
                item_id, output, ..
            } if item_id.contains("call_swarm_bg") => {
                swarm_output = Some(output.clone());
            }
            Event::SubagentCard {
                item_id,
                agent_id,
                background,
                ..
            } => live_cards.push((item_id.clone(), agent_id.clone(), *background)),
            _ => {}
        }
    }
    let output = swarm_output.expect("should have AgentSwarm ToolCallEnd");
    assert!(output.contains("2 subagents total"), "{output}");
    assert_eq!(
        output.matches("agent_id: ").count(),
        2,
        "listed per subagent: {output}"
    );
    assert_eq!(output.matches("task_id: ").count(), 2, "{output}");
    assert!(output.contains("status: running"), "{output}");
    assert!(
        output.contains("terminal notification") && output.contains("TaskWait"),
        "receipt should explain the notification semantics: {output}"
    );
    // Live: two background cards attach to the same tool card (same item_id), agent_ids distinct
    assert_eq!(live_cards.len(), 2, "two live cards: {live_cards:?}");
    assert!(live_cards.iter().all(|c| c.2), "background all true");
    assert_ne!(live_cards[0].1, live_cards[1].1, "agent_ids distinct");
    assert_eq!(
        live_cards[0].0, live_cards[1].0,
        "attached to the same tool card"
    );
    agent.shutdown();

    // Rollout: the AgentSwarm ToolCall record carries 2 agent_cards (background=true)
    let rollout_path = data_dir.join("sessions").join(format!("{sid}.jsonl"));
    let content = std::fs::read_to_string(&rollout_path).expect("rollout readable");
    let swarm_record = content
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|record| record["type"] == "tool_call" && record["tool"] == "AgentSwarm")
        .expect("rollout should have an AgentSwarm tool record");
    let agent_cards = swarm_record["agent_cards"]
        .as_array()
        .expect("agent_cards should be an array");
    assert_eq!(
        agent_cards.len(),
        2,
        "batch cards persisted: {swarm_record}"
    );
    assert!(
        agent_cards
            .iter()
            .all(|card| card["background"] == true && card["agent_id"].is_string()),
        "card metadata complete: {agent_cards:?}"
    );
    assert_eq!(
        swarm_record["agent_card"],
        serde_json::Value::Null,
        "single-card slot untouched"
    );

    // Simulate a restart and reopen the session: replay rebuilds both subagent cards and appends finished to each to settle the terminal state
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut collected = Vec::new();
    let mut finished_ids = std::collections::HashSet::new();
    while finished_ids.len() < 2 {
        assert!(
            Instant::now() < deadline,
            "replay should emit finished for both background subagents: {collected:#?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events2.recv()).await
        else {
            continue;
        };
        if let Event::ConversationStatus {
            agent_id,
            running: false,
            ..
        } = &event
        {
            finished_ids.insert(agent_id.clone());
        }
        collected.push(event);
    }
    let replay_cards: Vec<(String, String, bool)> = collected
        .iter()
        .filter_map(|e| match e {
            Event::SubagentCard {
                item_id,
                agent_id,
                background,
                ..
            } => Some((item_id.clone(), agent_id.clone(), *background)),
            _ => None,
        })
        .collect();
    assert_eq!(
        replay_cards.len(),
        2,
        "replay rebuilds two cards: {replay_cards:?}"
    );
    assert!(
        replay_cards.iter().all(|c| c.2),
        "replayed cards background=true"
    );
    assert_eq!(
        replay_cards[0].0, replay_cards[1].0,
        "both replayed cards attach to the same AgentSwarm tool card"
    );
    // The replayed cards' agent_id matches live; finished points at the same set of agent_ids
    for (_, agent_id, _) in &replay_cards {
        assert!(
            finished_ids.contains(agent_id),
            "{agent_id} should have a finished event"
        );
        assert!(
            live_cards.iter().any(|c| &c.1 == agent_id),
            "replayed card should share the live card's agent_id: {agent_id}"
        );
    }
    agent2.shutdown();
}

/// Subagent card metadata persisted with the rollout and rebuilt on replay
/// (A3e): run one background subagent → simulate a restart OpenSession replay
/// → the event stream should have SubagentCard (full meta, background=true),
/// followed by a ConversationStatus settled for that agent_id (background tasks
/// do not outlive the process, so replay settles the terminal state, keeping
/// the replayed card from spinning forever).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_background_card_replayed() {
    let (config_path, cwd, data_dir) = setup("subagent-replay");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let sid = new_session(&agent, cwd.clone()).await;

    // Run one background subagent to completion (waiting for the completion notification = the task has terminated)
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} BG", mock::SUBAGENT_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the notification"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        if let Event::TaskNotificationBatch { notifications, .. } = &event
            && !notifications.is_empty()
        {
            break;
        }
    }
    agent.shutdown();

    // Simulate a restart and reopen the session: replay rebuild
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    // The last event of the replay sequence is that agent's finished (after the closing TurnComplete)
    let collected = recv_until(&events2, Duration::from_secs(20), |e| {
        matches!(e, Event::ConversationStatus { running: false, .. })
    })
    .await;

    // SubagentCard: full meta, background=true
    let card = collected
        .iter()
        .find_map(|e| match e {
            Event::SubagentCard {
                agent_id,
                profile,
                description,
                model,
                background,
                ..
            } => Some((
                agent_id.clone(),
                profile.clone(),
                description.clone(),
                model.clone(),
                *background,
            )),
            _ => None,
        })
        .expect("replay should re-emit SubagentCard");
    assert!(card.4, "BG replay background should be true");
    assert_eq!(card.1, "explore");
    assert_eq!(card.2, "subagent selftest delegation");
    assert!(!card.3.is_empty(), "model should be non-empty");
    // The finished closing marker comes after the card and points at the same agent_id
    let card_pos = collected
        .iter()
        .position(|e| matches!(e, Event::SubagentCard { .. }))
        .expect("already asserted to exist");
    let has_finished = collected.iter().skip(card_pos).any(|e| {
        matches!(
            e,
            Event::ConversationStatus { agent_id, running: false, .. }
                if *agent_id == card.0
        )
    });
    assert!(
        has_finished,
        "replay should append finished to settle the terminal state: {collected:#?}"
    );
    agent2.shutdown();
}

/// SendMessage steering: the parent steers a running background LOOP child;
/// the wrapped message enters the child's conversation at its next step
/// boundary (visible in the child's follow-up request body).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_message_steers_running_subagent() {
    let (config_path, cwd, data_dir, log) = common::setup_with_log("subagent-steer");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} STEERBG", mock::SUBAGENT_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    // The parent delegates (running receipt), then calls SendMessage; the
    // receipt confirms delivery
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "steer delegation did not complete"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        if let Event::ToolCallEnd {
            output, is_error, ..
        } = &event
            && output.contains("Delivered to a")
        {
            assert!(!is_error, "SendMessage should succeed: {output}");
            break;
        }
    }

    // The child's follow-up request carries the wrapped steer message
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "the child never saw the steer");
        let saw = log.lock().unwrap().iter().any(|body| {
            body.contains("New message from the coordinating agent")
                && body.contains("STEER_MARKER_9")
        });
        if saw {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // The LOOP child never stops: cancel via the session token, then wrap up
    agent
        .ops
        .send(Op::Interrupt { session_id: sid })
        .await
        .unwrap();
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_subagent_executes_and_restores_direct_lineage() {
    let (config_path, cwd, data_dir) = setup("nested-subagent-lineage");
    let mut config = std::fs::read_to_string(&config_path).expect("config");
    config.push_str("\n[agents]\nmax_delegation_depth = 2\n");
    std::fs::write(&config_path, config).expect("nested depth config");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let sid = new_session(&agent, cwd.clone()).await;
    let events = run_turn(
        &agent,
        &sid,
        &format!("{} NESTED", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&events).expect("root Agent result");
    assert!(!is_error, "nested delegation failed: {output}");
    assert!(
        output.contains(mock::SUBAGENT_NESTED_CHILD_DONE),
        "{output}"
    );
    assert!(output.contains(mock::SUBAGENT_GRANDCHILD_DONE), "{output}");

    let agents_dir = data_dir.join("sessions").join(format!("{sid}.agents"));
    let mut child_id = None;
    let mut child_run_id = None;
    let mut grandchild_id = None;
    let mut grandchild_run_id = None;
    let mut child_raw = String::new();
    for entry in std::fs::read_dir(&agents_dir).expect("agent contexts") {
        let path = entry.expect("agent entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let raw = std::fs::read_to_string(&path).expect("agent JSONL");
        let meta: serde_json::Value =
            serde_json::from_str(raw.lines().next().expect("agent meta line")).expect("agent meta");
        if raw.contains("call_nested_agent") {
            assert_eq!(meta["parent_agent_id"].as_str(), Some(sid.as_str()));
            child_id = Some(meta["id"].as_str().expect("child id").to_string());
            child_run_id = Some(meta["run_id"].as_str().expect("child run").to_string());
            child_raw = raw;
        } else {
            grandchild_id = Some(meta["id"].as_str().expect("grandchild id").to_string());
            grandchild_run_id = Some(meta["run_id"].as_str().expect("grandchild run").to_string());
        }
    }
    let child_id = child_id.expect("direct child");
    let child_run_id = child_run_id.expect("child run");
    let grandchild_id = grandchild_id.expect("grandchild");
    let grandchild_run_id = grandchild_run_id.expect("grandchild run");
    assert_ne!(child_id, grandchild_id);
    assert!(child_run_id.starts_with("run-"));
    assert!(grandchild_run_id.starts_with("run-"));

    let nested_result = child_raw
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|record| record["type"] == "tool_call" && record["tool"] == "Agent")
        .expect("nested Agent result");
    assert_eq!(
        nested_result["agent_card"]["agent_id"].as_str(),
        Some(grandchild_id.as_str())
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ConversationUpdate {
            agent_id: Some(agent_id),
            item: pig_protocol::ConversationItem::Tool { agents, .. },
            ..
        } if agent_id == &child_id && agents.iter().any(|card| card.agent_id == grandchild_id)
    )));
    agent.shutdown();

    let connection = rusqlite::Connection::open(data_dir.join("store.sqlite")).expect("SQLite");
    connection
        .execute(
            "DELETE FROM background_tasks WHERE session_id = ?1",
            rusqlite::params![sid],
        )
        .expect("force orphan catalog recovery");
    drop(connection);

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir.clone());
    agent
        .ops
        .send(Op::OpenSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::ListAgents {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let catalog = recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::AgentCatalogChanged { agents, .. }
                if agents.iter().any(|entry| entry.agent_id == child_id)
                    && agents.iter().any(|entry| entry.agent_id == grandchild_id)
        )
    })
    .await;
    let entries = catalog
        .iter()
        .find_map(|event| match event {
            Event::AgentCatalogChanged { agents, .. }
                if agents.iter().any(|entry| entry.agent_id == grandchild_id) =>
            {
                Some(agents)
            }
            _ => None,
        })
        .expect("nested catalog");
    let child = entries
        .iter()
        .find(|entry| entry.agent_id == child_id)
        .expect("child catalog entry");
    assert_eq!(child.parent_id.as_deref(), Some(sid.as_str()));
    assert_eq!(child.run_id.as_deref(), Some(child_run_id.as_str()));
    assert_eq!(child.status, Some(pig_protocol::AgentRunStatus::Completed));
    let grandchild = entries
        .iter()
        .find(|entry| entry.agent_id == grandchild_id)
        .expect("grandchild catalog entry");
    assert_eq!(grandchild.parent_id.as_deref(), Some(child_id.as_str()));
    assert_eq!(
        grandchild.run_id.as_deref(),
        Some(grandchild_run_id.as_str())
    );
    assert_eq!(
        grandchild.status,
        Some(pig_protocol::AgentRunStatus::Completed)
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_swarm_executes_and_persists_all_cards() {
    let (config_path, cwd, data_dir) = setup("nested-swarm");
    let mut config = std::fs::read_to_string(&config_path).expect("config");
    config.push_str("\n[agents]\nmax_delegation_depth = 2\n");
    std::fs::write(&config_path, config).expect("nested depth config");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    let events = run_turn(
        &agent,
        &sid,
        &format!("{} NESTED_SWARM", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&events).expect("root Agent result");
    assert!(!is_error, "nested swarm failed: {output}");
    assert!(
        output.contains(mock::SUBAGENT_NESTED_CHILD_DONE),
        "{output}"
    );
    assert!(output.contains(mock::SUBAGENT_GRANDCHILD_DONE), "{output}");

    let agents_dir = data_dir.join("sessions").join(format!("{sid}.agents"));
    let contexts: Vec<_> = std::fs::read_dir(&agents_dir)
        .expect("agent contexts")
        .map(|entry| entry.expect("agent entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    assert_eq!(contexts.len(), 3, "one child and two grandchildren");
    let child_raw = contexts
        .iter()
        .map(|path| std::fs::read_to_string(path).expect("agent JSONL"))
        .find(|raw| raw.contains("call_nested_swarm"))
        .expect("child swarm context");
    let swarm_result = child_raw
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|record| record["type"] == "tool_call" && record["tool"] == "AgentSwarm")
        .expect("nested AgentSwarm result");
    let cards = swarm_result["agent_cards"].as_array().expect("swarm cards");
    assert_eq!(cards.len(), 2);
    assert!(cards.iter().all(|card| card["agent_id"].as_str().is_some()));
    agent.shutdown();
}

fn nested_tool_record(
    data_dir: &Path,
    sid: &str,
    parent_agent_id: &str,
    tool: &str,
) -> serde_json::Value {
    let path = agent_log_path(data_dir, sid, parent_agent_id);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|record| record["type"] == "tool_call" && record["tool"] == tool)
        .unwrap_or_else(|| panic!("missing nested {tool} record in {}", path.display()))
}

async fn collect_notifications_after_initial_turn(
    agent: &pig_core::AgentHandle,
    initial: &[Event],
    expected: usize,
) -> Vec<pig_protocol::TaskNotification> {
    let mut notifications: Vec<_> = initial
        .iter()
        .filter_map(|event| match event {
            Event::TaskNotificationBatch { notifications, .. } => Some(notifications.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    if notifications.len() >= expected {
        return notifications;
    }
    let mut turn_complete = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} nested notifications: {notifications:#?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match event {
            Event::TaskNotificationBatch {
                notifications: batch,
                ..
            } => {
                notifications.extend(batch);
                turn_complete = false;
            }
            Event::TurnComplete { .. } if notifications.len() >= expected => {
                turn_complete = true;
            }
            Event::Error { error, .. } => panic!("nested notification turn failed: {error:?}"),
            _ => {}
        }
        if notifications.len() >= expected && turn_complete {
            return notifications;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_background_agent_continues_child_and_falls_back_to_root() {
    let (config_path, cwd, data_dir, requests) = setup_with_log("nested-background-agent");
    let mut config = std::fs::read_to_string(&config_path).expect("config");
    config.push_str("\n[agents]\nmax_delegation_depth = 2\n");
    std::fs::write(&config_path, config).expect("nested depth config");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let events = run_turn(
        &agent,
        &sid,
        &format!("{} NESTED_BG", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&events).expect("root Agent result");
    assert!(!is_error, "nested background delegation failed: {output}");
    assert!(
        output.contains(mock::SUBAGENT_NESTED_CHILD_DONE),
        "{output}"
    );
    let child_id = parse_agent_id(output).to_string();
    let nested = nested_tool_record(&data_dir, &sid, &child_id, "Agent");
    assert!(
        nested["output"]
            .as_str()
            .expect("receipt")
            .contains("status: running")
    );
    let card = &nested["agent_card"];
    assert_eq!(card["background"], serde_json::json!(true));
    let grandchild_id = card["agent_id"]
        .as_str()
        .expect("grandchild id")
        .to_string();
    let grandchild_run_id = card["run_id"].as_str().expect("grandchild run").to_string();
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ConversationUpdate {
            agent_id: Some(agent_id),
            item: pig_protocol::ConversationItem::Tool { agents, .. },
            ..
        } if agent_id == &child_id && agents.iter().any(|card| {
            card.agent_id == grandchild_id
                && card.run_id.as_deref() == Some(grandchild_run_id.as_str())
                && !card.finished
        })
    )));

    let notifications = collect_notifications_after_initial_turn(&agent, &events, 1).await;
    assert_eq!(
        notifications.len(),
        1,
        "the grandchild must notify exactly once"
    );
    let notification = &notifications[0];
    assert_eq!(
        notification.agent_id.as_deref(),
        Some(grandchild_id.as_str())
    );
    assert_eq!(notification.status, pig_protocol::TaskStatus::Completed);
    let Some(pig_protocol::TaskNotificationOutput::File { path, .. }) = &notification.output else {
        panic!("grandchild notification should point to its result")
    };
    assert!(
        std::fs::read_to_string(path)
            .expect("grandchild result")
            .contains(mock::SUBAGENT_GRANDCHILD_DONE)
    );
    assert_eq!(
        requests
            .lock()
            .expect("request log")
            .iter()
            .filter(|request| request.contains(&notification.id))
            .count(),
        1,
        "the root model receives the fallback notification once"
    );

    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).expect("SQLite");
    let (parent, run, state): (String, String, String) = db
        .query_row(
            "SELECT parent_agent_id, run_id, state FROM background_tasks
             WHERE session_id = ?1 AND agent_id = ?2",
            rusqlite::params![sid, grandchild_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("grandchild task row");
    assert_eq!(parent, child_id);
    assert_eq!(run, grandchild_run_id);
    assert_eq!(state, "completed");
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_background_swarm_settles_each_grandchild_independently() {
    let (config_path, cwd, data_dir) = setup("nested-background-swarm");
    let mut config = std::fs::read_to_string(&config_path).expect("config");
    config.push_str("\n[agents]\nmax_delegation_depth = 2\n");
    std::fs::write(&config_path, config).expect("nested depth config");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let events = run_turn(
        &agent,
        &sid,
        &format!("{} NESTED_SWARM_BG", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&events).expect("root Agent result");
    assert!(!is_error, "nested background swarm failed: {output}");
    assert!(
        output.contains(mock::SUBAGENT_NESTED_CHILD_DONE),
        "{output}"
    );
    let child_id = parse_agent_id(output).to_string();
    let nested = nested_tool_record(&data_dir, &sid, &child_id, "AgentSwarm");
    assert_eq!(nested["agent_card"], serde_json::Value::Null);
    let cards = nested["agent_cards"].as_array().expect("swarm cards");
    assert_eq!(cards.len(), 2);
    assert!(cards.iter().all(|card| card["background"] == true));
    let expected: std::collections::HashMap<String, String> = cards
        .iter()
        .map(|card| {
            (
                card["agent_id"].as_str().expect("agent id").to_string(),
                card["run_id"].as_str().expect("run id").to_string(),
            )
        })
        .collect();

    let notifications = collect_notifications_after_initial_turn(&agent, &events, 2).await;
    assert_eq!(
        notifications.len(),
        2,
        "no duplicate or aggregate notification"
    );
    let notified: std::collections::HashSet<String> = notifications
        .iter()
        .map(|notification| notification.agent_id.clone().expect("grandchild id"))
        .collect();
    assert_eq!(notified, expected.keys().cloned().collect());
    for notification in &notifications {
        assert_eq!(notification.status, pig_protocol::TaskStatus::Completed);
        let Some(pig_protocol::TaskNotificationOutput::File { path, .. }) = &notification.output
        else {
            panic!("each grandchild needs its own result file")
        };
        assert!(
            std::fs::read_to_string(path)
                .expect("grandchild result")
                .contains(mock::SUBAGENT_GRANDCHILD_DONE)
        );
    }

    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).expect("SQLite");
    let mut statement = db
        .prepare(
            "SELECT agent_id, parent_agent_id, run_id, state FROM background_tasks
             WHERE session_id = ?1 AND agent_id IS NOT NULL",
        )
        .expect("task query");
    let rows = statement
        .query_map(rusqlite::params![sid], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .expect("task rows")
        .map(|row| row.expect("task row"))
        .collect::<Vec<_>>();
    for (agent_id, run_id) in expected {
        let row = rows
            .iter()
            .find(|row| row.0 == agent_id)
            .expect("grandchild row");
        assert_eq!(row.1, child_id);
        assert_eq!(row.2, run_id);
        assert_eq!(row.3, "completed");
    }
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_background_wait_delivers_to_direct_parent_without_root_duplicate() {
    let (config_path, cwd, data_dir) = setup("nested-background-wait");
    let mut config = std::fs::read_to_string(&config_path).expect("config");
    config.push_str("\n[agents]\nmax_delegation_depth = 2\n");
    std::fs::write(&config_path, config).expect("nested depth config");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let events = run_turn(
        &agent,
        &sid,
        &format!("{} NESTED_BG_WAIT", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::TaskNotificationBatch { .. }))
    );
    let (output, is_error) = agent_end(&events).expect("root Agent result");
    assert!(!is_error, "nested TaskWait scenario failed: {output}");
    let child_id = parse_agent_id(output).to_string();
    let nested = nested_tool_record(&data_dir, &sid, &child_id, "Agent");
    let task_id = nested["output"]
        .as_str()
        .expect("nested receipt")
        .lines()
        .find_map(|line| line.strip_prefix("task_id: "))
        .expect("nested task id")
        .to_string();
    let waited = nested_tool_record(&data_dir, &sid, &child_id, "TaskWait");
    assert!(
        waited["output"]
            .as_str()
            .expect("TaskWait output")
            .contains(&format!("task:{sid}:{task_id}:completed"))
    );

    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        let Ok(Ok(event)) =
            tokio::time::timeout(Duration::from_millis(100), agent.events.recv()).await
        else {
            continue;
        };
        assert!(!matches!(event, Event::TaskNotificationBatch { .. }));
    }
    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).expect("SQLite");
    let pending: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM task_notifications WHERE session_id = ?1 AND delivery_state = 'scheduled'",
            rusqlite::params![sid],
            |row| row.get(0),
        )
        .expect("pending notifications");
    assert_eq!(pending, 0, "TaskWait consumption must not wake the root");
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_background_restart_recovers_lost_run_with_direct_lineage() {
    let (config_path, cwd, data_dir) = setup("nested-background-restart");
    let mut config = std::fs::read_to_string(&config_path).expect("config");
    config.push_str("\n[agents]\nmax_delegation_depth = 2\n");
    std::fs::write(&config_path, config).expect("nested depth config");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let sid = new_session(&agent, cwd.clone()).await;
    let events = run_turn(
        &agent,
        &sid,
        &format!("{} NESTED_BG_LOOP", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&events).expect("root Agent result");
    assert!(!is_error, "nested running grandchild failed: {output}");
    let child_id = parse_agent_id(output).to_string();
    let nested = nested_tool_record(&data_dir, &sid, &child_id, "Agent");
    let grandchild_id = nested["agent_card"]["agent_id"]
        .as_str()
        .expect("grandchild id")
        .to_string();
    let grandchild_run_id = nested["agent_card"]["run_id"]
        .as_str()
        .expect("grandchild run")
        .to_string();
    agent.shutdown();
    drop(agent);

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    agent
        .ops
        .send(Op::OpenSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::ListAgents {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let events = recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(
            event,
            Event::AgentCatalogChanged { agents, .. }
                if agents.iter().any(|entry| {
                    entry.agent_id == grandchild_id
                        && entry.status == Some(pig_protocol::AgentRunStatus::Lost)
                })
        )
    })
    .await;
    let agents = events
        .iter()
        .find_map(|event| match event {
            Event::AgentCatalogChanged { agents, .. } => Some(agents),
            _ => None,
        })
        .expect("agent catalog");
    let grandchild = agents
        .iter()
        .find(|entry| entry.agent_id == grandchild_id)
        .expect("grandchild catalog entry");
    assert_eq!(grandchild.parent_id.as_deref(), Some(child_id.as_str()));
    assert_eq!(
        grandchild.run_id.as_deref(),
        Some(grandchild_run_id.as_str())
    );
    assert_eq!(grandchild.status, Some(pig_protocol::AgentRunStatus::Lost));
    assert!(
        grandchild.record_path.is_some(),
        "Lost child remains resumable"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_session_cleans_up_a_running_nested_background_agent() {
    let (config_path, cwd, data_dir) = setup("nested-background-delete");
    let mut config = std::fs::read_to_string(&config_path).expect("config");
    config.push_str("\n[agents]\nmax_delegation_depth = 2\n");
    std::fs::write(&config_path, config).expect("nested depth config");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    let events = run_turn(
        &agent,
        &sid,
        &format!("{} NESTED_BG_LOOP", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&events).expect("root Agent result");
    assert!(!is_error, "nested background setup failed: {output}");
    let child_id = parse_agent_id(output).to_string();
    let nested = nested_tool_record(&data_dir, &sid, &child_id, "Agent");
    let grandchild_id = nested["agent_card"]["agent_id"]
        .as_str()
        .expect("grandchild id")
        .to_string();
    let agents_dir = data_dir.join("sessions").join(format!("{sid}.agents"));
    assert!(agent_log_path(&data_dir, &sid, &grandchild_id).is_file());

    agent
        .ops
        .send(Op::DeleteSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::SessionList { sessions, .. }
                if !sessions.iter().any(|session| session.id == sid)
        )
    })
    .await;
    let cleanup_deadline = Instant::now() + Duration::from_secs(10);
    while agents_dir.exists() {
        assert!(
            Instant::now() < cleanup_deadline,
            "nested contexts remained after the background driver settled"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).expect("SQLite");
    let remaining: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM background_tasks WHERE session_id = ?1",
            rusqlite::params![sid],
            |row| row.get(0),
        )
        .expect("deleted task rows");
    assert_eq!(remaining, 0);

    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        let Ok(Ok(event)) =
            tokio::time::timeout(Duration::from_millis(100), agent.events.recv()).await
        else {
            continue;
        };
        assert!(!matches!(
            event,
            Event::TaskNotificationBatch { session_id, .. } if session_id == sid
        ));
    }
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_background_stop_then_resume_releases_writer_and_preserves_run_lineage() {
    let (config_path, cwd, data_dir) = setup("nested-background-stop-resume");
    let mut config = std::fs::read_to_string(&config_path).expect("config");
    config.push_str("\n[agents]\nmax_delegation_depth = 2\n");
    std::fs::write(&config_path, config).expect("nested depth config");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    let first = run_turn(
        &agent,
        &sid,
        &format!("{} NESTED_BG_STOP", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&first).expect("root Agent result");
    assert!(!is_error, "nested TaskStop scenario failed: {output}");
    let notifications = collect_notifications_after_initial_turn(&agent, &first, 1).await;
    assert_eq!(notifications.len(), 1);
    assert_eq!(notifications[0].status, pig_protocol::TaskStatus::Killed);
    let grandchild_id = notifications[0]
        .agent_id
        .clone()
        .expect("stopped grandchild id");

    let second = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME_NESTED {grandchild_id}", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (resumed, is_error) =
        agent_end_with(&second, "call_agent_2").expect("resumed grandchild result");
    assert!(!is_error, "the stopped writer must be released: {resumed}");
    assert!(resumed.contains(mock::SUBAGENT_RESUMED_DONE), "{resumed}");
    assert!(
        resumed.contains(&format!("agent_id: {grandchild_id}")),
        "{resumed}"
    );

    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).expect("SQLite");
    let mut statement = db
        .prepare(
            "SELECT parent_agent_id, run_id, state FROM background_tasks
             WHERE session_id = ?1 AND agent_id = ?2 ORDER BY task_id",
        )
        .expect("task query");
    let rows = statement
        .query_map(rusqlite::params![sid, grandchild_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("task rows")
        .map(|row| row.expect("task row"))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2, "stop and resume create distinct run records");
    assert_eq!(rows[0].2, "killed");
    assert_eq!(
        rows[1].0, sid,
        "the root is the resumed run's direct parent"
    );
    assert_eq!(rows[1].2, "completed");
    assert_ne!(rows[0].1, rows[1].1, "resume creates a new run id");
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_nested_parent_consumes_automatic_notification_without_waking_root() {
    let (config_path, cwd, data_dir) = setup("nested-background-steer");
    let mut config = std::fs::read_to_string(&config_path).expect("config");
    config.push_str("\n[agents]\nmax_delegation_depth = 2\n");
    std::fs::write(&config_path, config).expect("nested depth config");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;
    let events = run_turn(
        &agent,
        &sid,
        &format!("{} NESTED_BG_STEER", mock::SUBAGENT_TRIGGER),
        ExecMode::Auto,
        None,
    )
    .await;
    let (output, is_error) = agent_end(&events).expect("root Agent result");
    assert!(!is_error, "nested automatic notification failed: {output}");
    assert!(
        output.contains(mock::SUBAGENT_NESTED_CHILD_DONE),
        "{output}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::TaskNotificationBatch { .. }))
    );
    let child_id = parse_agent_id(output).to_string();
    let nested = nested_tool_record(&data_dir, &sid, &child_id, "Agent");
    let task_id = nested["output"]
        .as_str()
        .expect("nested receipt")
        .lines()
        .find_map(|line| line.strip_prefix("task_id: "))
        .expect("nested task id")
        .to_string();
    let notification_id = format!("task:{sid}:{task_id}:completed");
    let child_context =
        std::fs::read_to_string(agent_log_path(&data_dir, &sid, &child_id)).expect("child context");
    assert!(
        child_context.contains("New message from the coordinating agent")
            && child_context.contains(&notification_id),
        "the running direct parent receives the terminal notification"
    );

    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).expect("SQLite");
    let suppressed: bool = db
        .query_row(
            "SELECT terminal_notification_suppressed FROM background_tasks
             WHERE session_id = ?1 AND task_id = ?2",
            rusqlite::params![sid, task_id],
            |row| row.get(0),
        )
        .expect("nested task suppression");
    assert!(
        suppressed,
        "a consumed steer must not later fall back to root"
    );
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        let Ok(Ok(event)) =
            tokio::time::timeout(Duration::from_millis(100), agent.events.recv()).await
        else {
            continue;
        };
        assert!(!matches!(event, Event::TaskNotificationBatch { .. }));
    }
    agent.shutdown();
}
