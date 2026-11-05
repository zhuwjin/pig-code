use super::*;

use super::super::managed::*;

#[test]
fn agent_context_native_replay_roundtrip() {
    let dir = std::env::temp_dir().join(format!("pig-agent-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.jsonl");
    let meta = serde_json::json!({
        "type": "meta",
        "agent_id": "a1",
        "profile": "general-purpose",
        "description": "test",
        "model": "m",
        "provider": "p",
        "created_at": 1,
    });
    persist_agent_line(&path, &meta);
    let replay = pig_provider::AssistantReplay {
        origin: pig_provider::ReplayOrigin {
            api_format: pig_protocol::ApiFormat::AnthropicMessages,
            base_url: "http://localhost".into(),
            model: None,
            credential: None,
        },
        items: vec![
            pig_provider::AssistantReplayItem::AnthropicThinking {
                thinking: "signed".into(),
                signature: Some("signature".into()),
            },
            pig_provider::AssistantReplayItem::AnthropicRedactedThinking {
                data: "redacted".into(),
            },
            pig_provider::AssistantReplayItem::Text {
                text: "answer".into(),
                citations: vec![],
            },
        ],
    };
    let mut msg = ChatMsg::assistant_with_replay(
        "answer".into(),
        vec![],
        Some("signed".into()),
        Some(replay.clone()),
    );
    persist_agent_msg(&path, &mut msg);

    let (_, history) = crate::agent::read_agent(&path).expect("read agent context");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].replay, Some(replay));

    let legacy_path = dir.join("legacy.jsonl");
    persist_agent_line(&legacy_path, &meta);
    persist_agent_line(
        &legacy_path,
        &serde_json::json!({"type": "msg", "msg": ChatMsg::assistant("legacy".into(), vec![], None)}),
    );
    let (_, legacy) = crate::agent::read_agent(&legacy_path).expect("read legacy context");
    assert!(legacy[0].replay.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_context_reconcile_drops_unanswered_calls() {
    let dir = std::env::temp_dir().join(format!("pig-agent-reconcile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.jsonl");
    persist_agent_line(
        &path,
        &serde_json::json!({
            "type": "meta", "agent_id": "a1", "profile": "general-purpose",
            "description": "test", "model": "m", "provider": "p", "created_at": 1,
        }),
    );
    let first = ToolCall {
        id: "call_1".into(),
        name: "Read".into(),
        arguments: "{}".into(),
    };
    let second = ToolCall {
        id: "call_2".into(),
        name: "Write".into(),
        arguments: "{}".into(),
    };
    let replay = pig_provider::AssistantReplay {
        origin: pig_provider::ReplayOrigin {
            api_format: pig_protocol::ApiFormat::AnthropicMessages,
            base_url: "http://localhost".into(),
            model: None,
            credential: None,
        },
        items: vec![
            pig_provider::AssistantReplayItem::ToolCall {
                call: first.to_wire(),
            },
            pig_provider::AssistantReplayItem::ToolCall {
                call: second.to_wire(),
            },
        ],
    };
    let mut assistant =
        ChatMsg::assistant_with_replay(String::new(), vec![first, second], None, Some(replay));
    persist_agent_msg(&path, &mut assistant);
    let mut result = ChatMsg::tool_result("call_1", "done".into());
    persist_agent_msg(&path, &mut result);
    let mut user = ChatMsg::user("continue".into());
    persist_agent_msg(&path, &mut user);

    let (_, history) = crate::agent::read_agent(&path).expect("read agent context");
    let calls = history[0].tool_calls.as_ref().expect("answered call");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    let replay = history[0].replay.as_ref().expect("native replay");
    assert_eq!(replay.items.len(), 1);
    assert_eq!(history[2].role, "user");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The drive records the persisted message, not a pre-persist copy: a
/// media write failure degrades the live message before it is pushed, so
/// JSONL and live history stay identical.
#[test]
fn media_write_failure_degrades_live_message_and_jsonl() {
    let dir = std::env::temp_dir().join(format!("pig-agent-live-media-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.jsonl");
    persist_agent_line(
        &path,
        &serde_json::json!({
            "type": "meta", "agent_id": "a1", "profile": "explore",
            "description": "media failure", "model": "m", "provider": "p",
            "created_at": 1,
        }),
    );
    std::fs::write(path.with_extension("media"), b"not a directory").unwrap();
    let mut message = ChatMsg::tool_result("call_pdf", "Read PDF doc.pdf".into());
    message.documents = vec![pig_provider::ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: pig_utils::image::base64_encode(b"%PDF-1.4\n%%EOF\n"),
        filename: "doc.pdf".into(),
        label: Some("doc.pdf".into()),
        token_estimate: None,
    }];
    persist_agent_msg(&path, &mut message);
    assert!(message.documents.is_empty(), "live message degraded");
    assert!(
        message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("1 PDF document(s) could not be saved")),
        "{:?}",
        message.content
    );
    let (_, history) = crate::agent::read_agent(&path).expect("read agent context");
    assert_eq!(history[0].content, message.content);
    assert!(history[0].documents.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn child_context_write_failure_is_sticky_and_carries_resume_guidance() {
    let dir = std::env::temp_dir().join(format!(
        "pig-core-subagent-write-fail-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let jsonl = dir.join("a1-1.jsonl");
    // A directory at the context path makes every append fail.
    std::fs::create_dir_all(&jsonl).expect("occupy the context path");

    let mut drive = test_drive("a1-1");
    drive.jsonl = jsonl.clone();
    let mut first_message = ChatMsg::user("first".into());
    let first = persist_child_message(&mut drive, &mut first_message)
        .expect_err("a directory cannot be appended to");
    assert!(
        drive.persist_failure.as_deref() == Some(first.as_str()),
        "the failure is remembered"
    );
    let second = persist_child_message(&mut drive, &mut ChatMsg::user("second".into()))
        .expect_err("the sticky failure fails fast");
    assert_eq!(second, first, "later appends return the same failure");

    let result = child_persist_stop(&drive, &first, 2, (1, 2, 3));
    assert!(result.is_error && !result.cancelled && !result.completed);
    assert!(
        result.result_text.contains("Subagent a1-1"),
        "{}",
        result.result_text
    );
    assert!(
        result
            .result_text
            .contains("Agent(resume=\"a1-1\", prompt=\"...\")"),
        "{}",
        result.result_text
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A trusted project skill is visible in the child prompt and must be
/// loadable through the child's Skill tool; an untrusted workspace keeps
/// the same skill denied even if the model guesses its name.
#[tokio::test]
async fn child_skill_tool_honors_workspace_trust() {
    let dir = std::env::temp_dir().join(format!("pig-core-child-skill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".pigcode/skills/demo")).expect("skill dir");
    std::fs::write(
        dir.join(".pigcode/skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: project skill\n---\nPROJECT_SKILL_BODY\n",
    )
    .expect("skill file");

    let mut drive = test_drive("a1-1");
    drive.cwd = dir.clone();
    drive.plugin_tools = crate::plugins::PluginToolset::new(
        &crate::plugins::PluginRegistry::shared(dir.clone()),
        &crate::config::WorkspaceTrust::untrusted(&dir),
        None,
        "s-test",
    );
    drive.trusted = true;
    let (output, is_error) = run_child_skill(&drive).await;
    assert!(!is_error, "a trusted project skill must load: {output}");
    assert!(output.contains("PROJECT_SKILL_BODY"), "{output}");

    drive.trusted = false;
    let (output, is_error) = run_child_skill(&drive).await;
    assert!(
        is_error,
        "an untrusted project skill must stay denied: {output}"
    );
    assert!(
        !output.contains("PROJECT_SKILL_BODY"),
        "the denial does not expose the body: {output}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Execute the child's Skill tool for the project skill "demo" under the
/// drive's constructed tool set.
async fn run_child_skill(drive: &SubagentDrive) -> (String, bool) {
    let state = crate::task::SessionToolState::for_test();
    let mut tracker = ChangeTracker::default();
    let call = ToolCall {
        id: "call-skill".into(),
        name: "Skill".into(),
        arguments: serde_json::json!({"skill": "demo"}).to_string(),
    };
    let extra = drive.extra_tools();
    let ctx = ToolContext {
        cwd: &drive.cwd,
        tracker: &mut tracker,
        state: &state,
        fs_grant: None,
        sandbox_override: Default::default(),
        sandbox_profile: crate::task::sandbox_profile_for(PermissionSettings::default(), false),
        additional_permissions: Default::default(),
    };
    let (output, is_error, ..) = crate::tool::execute_with_extra(&call, ctx, &extra).await;
    (output, is_error)
}

#[tokio::test]
async fn child_dynamic_tools_use_the_child_model_and_agent_identity() {
    let root = std::env::temp_dir().join(format!(
        "pig-child-dynamic-tools-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let package = root.join("package");
    let data = root.join("data");
    std::fs::create_dir_all(&package).expect("package directory");
    std::fs::write(
        package.join("pig-plugin.json"),
        r#"{
          "apiVersion": 1,
          "id": "demo",
          "name": "Demo",
          "version": "1.0.0",
          "build": 1,
          "backend": "backend.ts",
          "minHostVersion": "1.0.0",
          "protocols": {"ui": 1, "services": 1},
          "capabilities": [],
          "contributions": {
            "actions": ["provide_tools"],
            "tools": [{
              "name": "DynamicRead",
              "description": "Read dynamic data",
              "read_only": true,
              "availability": "plugin",
              "parameters": {"type": "object"}
            }]
          }
        }"#,
    )
    .expect("manifest");
    std::fs::write(
        package.join("App.tsx"),
        "export default function App() { return null; }",
    )
    .expect("entry");
    let mut registry = crate::plugins::PluginRegistry::new(data.clone());
    let snapshot = registry.sync(vec![pig_protocol::PluginPackageSource {
        root: package,
        source: pig_protocol::PluginSource::User,
    }]);
    assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
    registry
        .set_enabled("demo", &snapshot.packages[0].digest, true)
        .expect("enable package");
    let mut drive = test_drive("a-dynamic");
    drive.plugin_tools = crate::plugins::PluginToolset::new(
        &registry.into_shared(),
        &crate::config::WorkspaceTrust::untrusted(&root),
        None,
        "s-test",
    );
    let dispatcher = drive.plugin_tools.dispatcher();
    let (events_tx, events_rx) = async_channel::unbounded();
    let selecting = tokio::spawn(async move {
        drive.select_plugin_tools(&events_tx).await;
        drive
    });
    let query = match events_rx.recv().await.expect("selection query") {
        Event::PluginToolsQuery { query } => query,
        other => panic!("unexpected event: {other:?}"),
    };
    assert_eq!(query.agent_id.as_deref(), Some("a-dynamic"));
    dispatcher.resolve_selection(pig_protocol::PluginToolsSelection {
        request_id: query.request_id,
        session_id: query.session_id,
        package: query.package,
        digest: query.digest,
        result: Ok(vec!["DynamicRead".into()]),
    });
    let drive = selecting.await.expect("selection task");
    assert_eq!(drive.tools.len(), 1);
    assert_eq!(drive.tools[0].name(), "DynamicRead");
    assert_eq!(drive.schemas.len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn managed_driver_catches_panic_and_settles_registry() {
    let state = crate::task::SessionToolState::for_test();
    let pending = test_pending();
    let drive = test_drive("a1-1");
    let task_id = crate::task::register_agent_task(
        &state,
        "subagent explore: panic".into(),
        CancellationToken::new(),
        drive.agent_id.clone(),
    );
    let (tx, rx) = async_channel::unbounded();
    let writer = state
        .try_claim_agent_writer(&drive.agent_id)
        .expect("writer");
    let progress = ProgressSink::Background {
        task_id: task_id.clone(),
    };
    let life = ChildLifecycle::new(
        &state,
        &pending,
        "s-test",
        tx,
        &drive,
        writer,
        &progress,
        Some(task_id.clone()),
    );
    let cancel = CancellationToken::new();
    let outcome = drive_managed(
        life,
        &cancel,
        std::future::ready(Some(SubagentSlot::Test)),
        async { panic!("driver boom") },
    )
    .await;
    assert!(outcome.result.is_error && !outcome.result.cancelled);
    assert!(
        outcome.result.result_text.contains("panicked: driver boom"),
        "{}",
        outcome.result.result_text
    );
    let status = state
        .tasks
        .lock()
        .expect("task registry lock")
        .iter()
        .find(|entry| entry.id == task_id)
        .expect("entry")
        .status;
    assert!(
        !matches!(status, pig_protocol::TaskStatus::Running),
        "a panicking driver must not leave a Running task: {status:?}"
    );
    assert!(!state.agent_writer_active(&drive.agent_id));
    let finished = rx
        .try_recv()
        .ok()
        .is_some_and(|event| matches!(event, Event::SubagentActivity { finished: true, .. }));
    assert!(finished, "the live panel must settle on panic");
}

/// A panic outside the run future (here: while waiting for a slot) still
/// settles the registry and sends exactly one background failure
/// notification through the Drop fallback.
#[tokio::test]
async fn background_driver_panic_before_run_notifies_once() {
    let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = crate::task::SessionToolState::new(
        "s-test".into(),
        task_tx,
        vec![std::env::temp_dir()],
        vec![],
    );
    let pending = test_pending();
    let drive = test_drive("a2-3");
    let task_id = crate::task::register_agent_task(
        &state,
        "subagent explore: panic before run".into(),
        CancellationToken::new(),
        drive.agent_id.clone(),
    );
    let writer = state
        .try_claim_agent_writer(&drive.agent_id)
        .expect("writer");
    let progress = ProgressSink::Background {
        task_id: task_id.clone(),
    };
    let (tx, _rx) = async_channel::unbounded();
    let life = ChildLifecycle::new(
        &state,
        &pending,
        "s-test",
        tx,
        &drive,
        writer,
        &progress,
        Some(task_id.clone()),
    );
    let cancel = CancellationToken::new();
    let outcome = drive_managed(
        life,
        &cancel,
        async { panic!("acquire boom") },
        std::future::ready(SubagentDriveResult {
            status_line: "completed (1 steps)".into(),
            result_text: "done".into(),
            turns: 1,
            completed: true,
            is_error: false,
            usage: (0, 0, 0),
            cancelled: false,
        }),
    )
    .await;
    assert!(
        outcome
            .result
            .result_text
            .contains("panicked: acquire boom"),
        "{}",
        outcome.result.result_text
    );
    let status = state
        .tasks
        .lock()
        .expect("task registry lock")
        .iter()
        .find(|entry| entry.id == task_id)
        .expect("entry")
        .status;
    assert!(!matches!(status, pig_protocol::TaskStatus::Running));
    let changes = std::iter::from_fn(|| task_rx.try_recv().ok()).collect::<Vec<_>>();
    let notification = changes
        .iter()
        .find_map(|change| match change {
            crate::task::TaskChange::Terminal { notification, .. } => Some(notification),
            _ => None,
        })
        .expect("one failure notification");
    assert_eq!(notification.status, pig_protocol::TaskStatus::Failed);
    assert!(
        notification.body.contains("terminated unexpectedly"),
        "{}",
        notification.body
    );
    assert_eq!(
        changes
            .iter()
            .filter(|change| matches!(change, crate::task::TaskChange::Terminal { .. }))
            .count(),
        1,
        "a panic must not produce duplicate notifications"
    );
    assert!(!state.agent_writer_active(&drive.agent_id));
}

#[tokio::test]
async fn queued_cancellation_settles_as_killed_not_running() {
    let state = crate::task::SessionToolState::for_test();
    let pending = test_pending();
    let drive = test_drive("a2-2");
    let task_id = crate::task::register_agent_task_queued(
        &state,
        "subagent explore: queued".into(),
        CancellationToken::new(),
        drive.agent_id.clone(),
    );
    let life = test_lifecycle(&state, &pending, &drive, Some(task_id.clone()));
    let cancel = CancellationToken::new();
    let outcome = drive_managed(
        life,
        &cancel,
        std::future::ready(None),
        std::future::ready(cancelled_drive_result()),
    )
    .await;
    assert!(outcome.result.cancelled);
    let (status, ended_at) = {
        let tasks = state.tasks.lock().expect("task registry lock");
        let entry = tasks
            .iter()
            .find(|entry| entry.id == task_id)
            .expect("entry");
        (entry.status, entry.ended_at)
    };
    assert!(
        matches!(status, pig_protocol::TaskStatus::Killed),
        "queued cancellation must settle Killed, not Running: {status:?}"
    );
    assert!(ended_at.is_some());
    assert!(!state.agent_writer_active(&drive.agent_id));
}

/// Foreground swarm beyond the slot cap: the running children see the
/// parent cancel, and the queued children leave the queue and settle
/// Cancelled/Killed instead of remaining Running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swarm_beyond_slot_count_cancellation_settles_every_child() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let limit = crate::task::MAX_CONCURRENT_SUBAGENTS;
    let state = crate::task::SessionToolState::for_test();
    let pending = test_pending();
    let semaphore: &'static Arc<tokio::sync::Semaphore> =
        Box::leak(Box::new(Arc::new(tokio::sync::Semaphore::new(limit))));
    let cancel = CancellationToken::new();
    let started = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for index in 0..(limit + 2) {
        let drive = test_drive(&format!("a{index}-1"));
        let task_id = crate::task::register_agent_task(
            &state,
            format!("subagent explore: child {index}"),
            CancellationToken::new(),
            drive.agent_id.clone(),
        );
        let life = test_lifecycle(&state, &pending, &drive, Some(task_id.clone()));
        let semaphore = semaphore.clone();
        let acquire_cancel = cancel.clone();
        let drive_cancel = cancel.clone();
        let run_cancel = cancel.clone();
        let started = started.clone();
        handles.push(tokio::spawn(async move {
            let acquire = async move {
                tokio::select! {
                    biased;
                    _ = acquire_cancel.cancelled() => None,
                    permit = semaphore.acquire_owned() => {
                        permit.ok().map(SubagentSlot::TestPermit)
                    }
                }
            };
            let outcome = drive_managed(life, &drive_cancel, acquire, async move {
                started.fetch_add(1, Ordering::SeqCst);
                run_cancel.cancelled().await;
                cancelled_drive_result()
            })
            .await;
            (outcome, task_id)
        }));
    }
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while started.load(Ordering::SeqCst) < limit {
        assert!(
            Instant::now() < deadline,
            "only {} children acquired a slot",
            started.load(Ordering::SeqCst)
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    cancel.cancel();
    let joined = futures_util::future::join_all(handles).await;
    for joined in joined {
        let (outcome, task_id) = joined.expect("child task");
        assert!(outcome.result.cancelled);
        let status = state
            .tasks
            .lock()
            .expect("task registry lock")
            .iter()
            .find(|entry| entry.id == task_id)
            .expect("entry")
            .status;
        assert!(
            !matches!(status, pig_protocol::TaskStatus::Running),
            "cancelled child {task_id} is still Running"
        );
    }
    assert_eq!(started.load(Ordering::SeqCst), limit);
    assert_eq!(semaphore.available_permits(), limit);
}

#[tokio::test]
async fn child_termination_drops_only_its_approvals() {
    let state = crate::task::SessionToolState::for_test();
    let pending = test_pending();
    let drive = test_drive("a3-3");
    let task_id = crate::task::register_agent_task(
        &state,
        "subagent explore: approvals".into(),
        CancellationToken::new(),
        drive.agent_id.clone(),
    );
    let own_id = format!("s-test-{task_id}-approval-{task_id}-c1-call");
    let other_id = "s-test-other-approval-other-c1-call".to_string();
    {
        let mut pending = pending.lock().expect("pending lock");
        let (own_tx, _own_rx) = oneshot::channel();
        let (other_tx, _other_rx) = oneshot::channel();
        pending.insert(own_id.clone(), own_tx);
        pending.insert(other_id.clone(), other_tx);
    }
    let life = test_lifecycle(&state, &pending, &drive, Some(task_id));
    let cancel = CancellationToken::new();
    let outcome = drive_managed(
        life,
        &cancel,
        std::future::ready(Some(SubagentSlot::Test)),
        std::future::ready(SubagentDriveResult {
            status_line: "failed: test".into(),
            result_text: "test".into(),
            turns: 1,
            completed: false,
            is_error: true,
            usage: (0, 0, 0),
            cancelled: false,
        }),
    )
    .await;
    assert!(outcome.result.is_error);
    let pending = pending.lock().expect("pending lock");
    assert!(
        !pending.contains_key(&own_id),
        "own approval must be removed"
    );
    assert!(
        pending.contains_key(&other_id),
        "another child/session approval must stay"
    );
}

#[test]
fn failure_receipts_keep_agent_id_and_resume_guidance() {
    let result = SubagentDriveResult {
        status_line: "failed: reached the maximum turn count 20".into(),
        result_text: "Reached the maximum turn count (20). partial".into(),
        turns: 20,
        completed: false,
        is_error: true,
        usage: (1, 2, 3),
        cancelled: false,
    };
    let note = subagent_receipt("a7-7", "explore", &result);
    assert!(note.contains("agent_id: a7-7"), "{note}");
    assert!(note.contains("status: failed"), "{note}");
    assert!(
        note.contains("resume_hint: continue this subagent with Agent(resume=\"a7-7\""),
        "{note}"
    );
    assert!(note.contains("Reached the maximum turn count"), "{note}");

    let no_text = SubagentDriveResult {
        status_line: "failed: subagent produced no final text".into(),
        result_text: "The subagent produced no final text".into(),
        turns: 3,
        completed: true,
        is_error: true,
        usage: (0, 0, 0),
        cancelled: false,
    };
    assert!(
        subagent_receipt("a7-7", "explore", &no_text).contains("status: failed"),
        "an error result is failed even when the loop reached a natural stop"
    );
}

#[test]
fn max_turn_background_notification_is_failed_with_resume_guidance() {
    let state = crate::task::SessionToolState::for_test();
    let pending = test_pending();
    let drive = test_drive("a8-8");
    let task_id = crate::task::register_agent_task(
        &state,
        "subagent explore: max turns".into(),
        CancellationToken::new(),
        drive.agent_id.clone(),
    );
    let mut life = test_lifecycle(&state, &pending, &drive, Some(task_id.clone()));
    let result = SubagentDriveResult {
        status_line: "failed: reached the maximum turn count 20".into(),
        result_text: "Reached the maximum turn count (20). partial".into(),
        turns: 20,
        completed: false,
        is_error: true,
        usage: (0, 0, 0),
        cancelled: false,
    };
    life.settle(&result);
    let tasks = state.tasks.lock().expect("task registry lock");
    let entry = tasks
        .iter()
        .find(|entry| entry.id == task_id)
        .expect("entry");
    let notification = crate::task::notification::terminal_notification("test", entry);
    assert_eq!(notification.status, pig_protocol::TaskStatus::Failed);
    assert!(notification.turns == Some(20));
    assert!(
        notification.body.contains("Agent(resume=\"a8-8\""),
        "{}",
        notification.body
    );
}
