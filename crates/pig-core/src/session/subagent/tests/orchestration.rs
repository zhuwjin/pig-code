use super::*;
use std::time::Instant;

use super::super::managed::*;

#[test]
fn agent_context_native_replay_roundtrip() {
    let dir = std::env::temp_dir().join(format!("pig-agent-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.jsonl");
    persist_agent_meta(&path, "a1", "s-test", "general-purpose", "test");
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

    let plain_path = dir.join("plain.jsonl");
    persist_agent_meta(&plain_path, "a1", "s-test", "general-purpose", "test");
    let mut plain = ChatMsg::assistant("plain".into(), vec![], None);
    persist_agent_msg(&plain_path, &mut plain);
    let (_, plain) = crate::agent::read_agent(&plain_path).expect("read plain context");
    assert!(plain[0].replay.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_context_rejects_boundary_after_unanswered_calls() {
    let dir = std::env::temp_dir().join(format!("pig-agent-reconcile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.jsonl");
    persist_agent_meta(&path, "a1", "s-test", "general-purpose", "test");
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
    let result_call = ToolCall {
        id: "call_1".into(),
        name: "Read".into(),
        arguments: "{}".into(),
    };
    persist_agent_tool(&path, &result_call, &mut result);
    let mut user = ChatMsg::user("continue".into());
    persist_agent_msg(&path, &mut user);

    let error = crate::agent::read_agent(&path).expect_err("unfinished round before user");
    assert!(error.contains("unfinished tool round"));
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
    persist_agent_meta(&path, "a1", "s-test", "explore", "media failure");
    std::fs::write(path.with_extension("media"), b"not a directory").unwrap();
    let mut message = ChatMsg::tool_result("call_pdf", "Read PDF doc.pdf".into());
    message.documents = vec![pig_provider::ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: pig_utils::image::base64_encode(b"%PDF-1.4\n%%EOF\n"),
        filename: "doc.pdf".into(),
        label: Some("doc.pdf".into()),
        token_estimate: None,
    }];
    let call = ToolCall {
        id: "call_pdf".into(),
        name: "ReadMediaFile".into(),
        arguments: "{}".into(),
    };
    let mut assistant = ChatMsg::assistant(String::new(), vec![call.clone()], None);
    persist_agent_msg(&path, &mut assistant);
    persist_agent_tool(&path, &call, &mut message);
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
    assert_eq!(history[1].content, message.content);
    assert!(history[1].documents.is_empty());
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

    let mut drive = test_child_session("a1-1");
    drive.subagent.as_mut().expect("child").jsonl = jsonl.clone();
    let mut first_message = ChatMsg::user("first".into());
    let first = persist_child_message(&mut drive, &mut first_message)
        .expect_err("a directory cannot be appended to");
    assert!(
        drive
            .subagent
            .as_ref()
            .expect("child")
            .persist_failure
            .as_deref()
            == Some(first.as_str()),
        "the failure is remembered"
    );
    let second = persist_child_message(&mut drive, &mut ChatMsg::user("second".into()))
        .expect_err("the sticky failure fails fast");
    assert_eq!(second, first, "later appends return the same failure");

    let result = child_persist_stop(&drive, &first, 2, (1, 2, 3));
    assert!(result.is_error() && !result.is_cancelled() && !result.is_completed());
    assert!(result.text().contains("Subagent a1-1"), "{}", result.text());
    assert!(
        result
            .text()
            .contains("Agent(resume=\"a1-1\", prompt=\"...\")"),
        "{}",
        result.text()
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

    let mut drive = test_child_session("a1-1");
    drive.agent.cwd = dir.clone();
    drive.plugin_tools = crate::plugins::PluginToolset::new(
        &crate::plugins::PluginRegistry::shared(dir.clone()),
        &crate::config::WorkspaceTrust::untrusted(&dir),
        None,
        "s-test",
    );
    drive.subagent.as_mut().expect("child").trusted = true;
    let (output, is_error) = run_child_skill(&drive).await;
    assert!(!is_error, "a trusted project skill must load: {output}");
    assert!(output.contains("PROJECT_SKILL_BODY"), "{output}");

    drive.subagent.as_mut().expect("child").trusted = false;
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
async fn run_child_skill(drive: &Session) -> (String, bool) {
    let state = crate::task::SessionToolState::for_test();
    let mut tracker = ChangeTracker::default();
    let call = ToolCall {
        id: "call-skill".into(),
        name: "Skill".into(),
        arguments: serde_json::json!({"skill": "demo"}).to_string(),
    };
    let extra = drive.child_extra_tools();
    let ctx = ToolContext {
        cwd: &drive.agent.cwd,
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
    std::fs::write(package.join("backend.ts"), "export {};").expect("backend entry");
    let mut registry = crate::plugins::PluginRegistry::new(data.clone());
    let snapshot = registry.sync(vec![pig_protocol::PluginPackageSource {
        root: package,
        source: pig_protocol::PluginSource::User,
    }]);
    assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
    registry
        .set_enabled("demo", &snapshot.packages[0].digest, true)
        .expect("enable package");
    let mut drive = test_child_session("a-dynamic");
    drive.plugin_tools = crate::plugins::PluginToolset::new(
        &registry.into_shared(),
        &crate::config::WorkspaceTrust::untrusted(&root),
        None,
        "s-test",
    );
    let dispatcher = drive.plugin_tools.dispatcher();
    let (events_tx, events_rx) = async_channel::unbounded();
    let selecting = tokio::spawn(async move {
        drive.select_child_plugin_tools(&events_tx).await;
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
    assert_eq!(drive.agent.tools.len(), 1);
    assert_eq!(drive.agent.tools[0].name(), "DynamicRead");
    assert_eq!(drive.agent.schemas.len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn managed_driver_catches_panic_and_settles_registry() {
    let state = crate::task::SessionToolState::for_test();
    let pending = test_pending();
    let drive = test_child_session("a1-1");
    let task_id = crate::task::register_agent_task(
        &state,
        "subagent explore: panic".into(),
        CancellationToken::new(),
        drive.agent.id.clone(),
    );
    let (tx, rx) = async_channel::unbounded();
    let writer = state
        .try_claim_agent_writer(&drive.agent.id)
        .expect("writer");
    let life = AgentRunSupervisor::new(
        &state,
        &pending,
        "s-test",
        tx,
        &drive,
        writer,
        Some(task_id.clone()),
    );
    let cancel = CancellationToken::new();
    let outcome = life
        .drive(
            &cancel,
            std::future::ready(Some(SubagentSlot::Test)),
            async { panic!("driver boom") },
        )
        .await;
    assert!(outcome.result.is_error() && !outcome.result.is_cancelled());
    assert_eq!(outcome.result.agent_id.as_deref(), Some("a1-1"));
    assert!(outcome.result.run_id.is_some());
    assert!(
        outcome.result.text().contains("panicked: driver boom"),
        "{}",
        outcome.result.text()
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
    assert!(!state.agent_writer_active(&drive.agent.id));
    let finished = rx
        .try_recv()
        .ok()
        .is_some_and(|event| matches!(event, Event::ConversationStatus { running: false, .. }));
    assert!(finished, "the live panel must settle on panic");
}

#[tokio::test]
async fn uncooperative_local_run_is_dropped_after_stop_grace() {
    struct SetOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    let state = crate::task::SessionToolState::for_test();
    let pending = test_pending();
    let drive = test_child_session("a-stuck");
    let task_id = crate::task::register_agent_task(
        &state,
        "subagent explore: stuck".into(),
        CancellationToken::new(),
        drive.agent.id.clone(),
    );
    let (tx, _rx) = async_channel::unbounded();
    let writer = state
        .try_claim_agent_writer(&drive.agent.id)
        .expect("writer");
    let life = AgentRunSupervisor::new(
        &state,
        &pending,
        "s-test",
        tx,
        &drive,
        writer,
        Some(task_id.clone()),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mark_drop = SetOnDrop(dropped.clone());
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        life.drive(
            &cancel,
            std::future::ready(Some(SubagentSlot::Test)),
            async move {
                let _mark_drop = mark_drop;
                std::future::pending::<SubagentResult>().await
            },
        ),
    )
    .await
    .expect("forced stop must settle");
    assert!(outcome.result.is_cancelled());
    assert!(outcome.result.stop_reason.contains("grace period"));
    assert!(dropped.load(std::sync::atomic::Ordering::Acquire));
    assert!(!state.agent_writer_active(&drive.agent.id));
    let status = state
        .tasks
        .lock()
        .expect("task registry lock")
        .iter()
        .find(|entry| entry.id == task_id)
        .expect("entry")
        .status;
    assert_eq!(status, pig_protocol::TaskStatus::Killed);
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
    let drive = test_child_session("a2-3");
    let task_id = crate::task::register_agent_task(
        &state,
        "subagent explore: panic before run".into(),
        CancellationToken::new(),
        drive.agent.id.clone(),
    );
    let writer = state
        .try_claim_agent_writer(&drive.agent.id)
        .expect("writer");
    let (tx, _rx) = async_channel::unbounded();
    let life = AgentRunSupervisor::new(
        &state,
        &pending,
        "s-test",
        tx,
        &drive,
        writer,
        Some(task_id.clone()),
    );
    let cancel = CancellationToken::new();
    let outcome = life
        .drive(
            &cancel,
            async { panic!("acquire boom") },
            std::future::ready(SubagentResult::child(
                crate::session::AgentRunStatus::Completed,
                "completed".into(),
                "done".into(),
                1,
                (0, 0, 0),
            )),
        )
        .await;
    assert!(
        outcome.result.text().contains("panicked: acquire boom"),
        "{}",
        outcome.result.text()
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
    assert!(!state.agent_writer_active(&drive.agent.id));
}

#[tokio::test]
async fn queued_cancellation_settles_as_killed_not_running() {
    let state = crate::task::SessionToolState::for_test();
    let pending = test_pending();
    let mut drive = test_child_session("a2-2");
    let task_id = crate::task::register_agent_task_queued(
        &state,
        "subagent explore: queued".into(),
        CancellationToken::new(),
        drive.agent.id.clone(),
    );
    let life = test_lifecycle(&state, &pending, &mut drive, Some(task_id.clone()));
    let cancel = CancellationToken::new();
    let outcome = life
        .drive(
            &cancel,
            std::future::ready(None),
            std::future::ready(cancelled_subagent_result()),
        )
        .await;
    assert!(outcome.result.is_cancelled());
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
    assert!(!state.agent_writer_active(&drive.agent.id));
}

/// Foreground swarm beyond the slot cap: the running children see the
/// parent cancel, and the queued children leave the queue and settle
/// Cancelled/Killed instead of remaining Running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swarm_beyond_slot_count_cancellation_settles_every_child() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let limit = crate::agent::MAX_CONCURRENT_AGENT_RUNS;
    let state = crate::task::SessionToolState::for_test();
    let pending = test_pending();
    let semaphore: &'static Arc<tokio::sync::Semaphore> =
        Box::leak(Box::new(Arc::new(tokio::sync::Semaphore::new(limit))));
    let cancel = CancellationToken::new();
    let started = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for index in 0..(limit + 2) {
        let mut drive = test_child_session(&format!("a{index}-1"));
        let task_id = crate::task::register_agent_task(
            &state,
            format!("subagent explore: child {index}"),
            CancellationToken::new(),
            drive.agent.id.clone(),
        );
        let life = test_lifecycle(&state, &pending, &mut drive, Some(task_id.clone()));
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
            let outcome = life
                .drive(&drive_cancel, acquire, async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    run_cancel.cancelled().await;
                    cancelled_subagent_result()
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
        assert!(outcome.result.is_cancelled());
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
    let mut drive = test_child_session("a3-3");
    let task_id = crate::task::register_agent_task(
        &state,
        "subagent explore: approvals".into(),
        CancellationToken::new(),
        drive.agent.id.clone(),
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
    let life = test_lifecycle(&state, &pending, &mut drive, Some(task_id));
    let cancel = CancellationToken::new();
    let outcome = life
        .drive(
            &cancel,
            std::future::ready(Some(SubagentSlot::Test)),
            std::future::ready(SubagentResult::child(
                crate::session::AgentRunStatus::Failed,
                "test".into(),
                "test".into(),
                1,
                (0, 0, 0),
            )),
        )
        .await;
    assert!(outcome.result.is_error());
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

#[tokio::test]
async fn local_backend_rejects_external_reconnect_before_running() {
    use crate::agent::AgentExecutionBackend as _;

    let child = test_child_session("a-reconnect-1");
    let identity = crate::agent::AgentRunIdentity {
        session_id: child.event_session_id().to_string(),
        agent_id: child.agent.id.clone(),
        run_id: child.agent.run_id().expect("child run"),
    };
    let (events, _receiver) = async_channel::unbounded();
    let mut request = crate::agent::AgentExecutionRequest::new(
        identity.clone(),
        events,
        CancellationToken::new(),
        crate::agent::AgentExecutionActivation::Reconnect,
    );
    request.cursor = Some(crate::agent::AgentEventCursor {
        run: identity.clone(),
        sequence: 1,
    });
    let result = Box::new(crate::agent::LocalAgentExecutionBackend::new(child))
        .execute(request)
        .await;
    assert!(matches!(
        result,
        Err(crate::agent::AgentBackendError::UnsupportedCapability {
            capability: crate::agent::AgentExecutionCapability::Reconnect,
            ..
        })
    ));
}

#[test]
fn failure_receipts_keep_agent_id_and_resume_guidance() {
    let result = SubagentResult::child(
        crate::session::AgentRunStatus::Failed,
        "reached the maximum turn count 20".into(),
        "Reached the maximum turn count (20). partial".into(),
        20,
        (1, 2, 3),
    );
    let note = subagent_receipt("a7-7", "explore", &result);
    assert!(note.contains("agent_id: a7-7"), "{note}");
    assert!(note.contains("status: failed"), "{note}");
    assert!(
        note.contains("resume_hint: continue this subagent with Agent(resume=\"a7-7\""),
        "{note}"
    );
    assert!(note.contains("Reached the maximum turn count"), "{note}");

    let no_text = SubagentResult::child(
        crate::session::AgentRunStatus::Failed,
        "subagent produced no final text".into(),
        "The subagent produced no final text".into(),
        3,
        (0, 0, 0),
    );
    assert!(
        subagent_receipt("a7-7", "explore", &no_text).contains("status: failed"),
        "an error result is failed even when the loop reached a natural stop"
    );
}

#[test]
fn max_turn_background_notification_is_failed_with_resume_guidance() {
    let state = crate::task::SessionToolState::for_test();
    let pending = test_pending();
    let mut drive = test_child_session("a8-8");
    let task_id = crate::task::register_agent_task(
        &state,
        "subagent explore: max turns".into(),
        CancellationToken::new(),
        drive.agent.id.clone(),
    );
    let mut life = test_lifecycle(&state, &pending, &mut drive, Some(task_id.clone()));
    let result = SubagentResult::child(
        crate::session::AgentRunStatus::Failed,
        "reached the maximum turn count 20".into(),
        "Reached the maximum turn count (20). partial".into(),
        20,
        (0, 0, 0),
    );
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

#[tokio::test]
async fn child_parallel_tools_preserve_call_order() {
    let (session, parent, root) = swarm_test_session("parallel-tools");
    let profile = crate::agent::builtin_profiles()
        .into_iter()
        .find(|profile| profile.name == "general-purpose")
        .expect("general profile");
    let prep = crate::agent::prepare_fresh_child(
        &crate::agent::ChildPrepCtx {
            cwd: &session.agent.cwd,
            data_dir: &session.data_dir,
            git_snapshot: None,
            skills_prompt: "",
            agents_prompt: "",
            runtime: None,
            parent_config: &parent,
            session_id: &session.agent.id,
            parent_agent_id: &session.agent.id,
            parent_delegation_depth: 0,
            max_delegation_depth: 1,
            max_agents_per_session: 64,
            allowed_subagents: None,
            child_cwd: None,
            worktree: false,
            parent_permissions: session.permissions,
            parent_history: &[],
            profiles_summary: "",
            state: &session.state,
            mcp: None,
            plugin_tools: session.plugin_tools.clone(),
        },
        profile,
        parent.clone(),
        pig_provider::mock::PREFIX_PARALLEL_SCENARIO,
        "parallel child",
        &mut 0,
    )
    .expect("parallel child preparation");
    let mut child = Session::new_subagent(
        prep,
        &session,
        ProgressSink::Foreground {
            parent_item_id: "parent".into(),
        },
        SubagentMode::Foreground,
    );
    let writer = child
        .subagent
        .as_mut()
        .expect("child")
        .take_writer()
        .expect("writer");
    let (tx, _rx) = async_channel::unbounded();
    let mut life = AgentRunSupervisor::new(
        &child.state,
        &test_pending(),
        child.event_session_id(),
        tx.clone(),
        &child,
        writer,
        None,
    );
    let result = child
        .run_subagent_task(&tx, &CancellationToken::new())
        .await;
    assert_eq!(result.status, crate::session::AgentRunStatus::Completed);
    assert!(result.text().contains("MOCK_PREFIX_PARALLEL_DONE"));
    let calls = child
        .agent
        .history
        .iter()
        .find_map(|message| message.tool_calls.as_ref())
        .expect("assistant tool calls");
    assert_eq!(calls.len(), 2);
    let results: Vec<_> = child
        .agent
        .history
        .iter()
        .filter(|message| message.role == "tool")
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].tool_call_id.as_deref(),
        Some(calls[0].id.as_str())
    );
    assert_eq!(
        results[1].tool_call_id.as_deref(),
        Some(calls[1].id.as_str())
    );
    let raw = std::fs::read_to_string(&child.subagent.as_ref().expect("child").jsonl)
        .expect("child JSONL");
    assert_eq!(raw.matches("\"type\":\"tool_execution_start\"").count(), 2);
    assert_eq!(raw.matches("\"type\":\"tool_call\"").count(), 2);
    life.settle(&result);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn child_structured_output_uses_the_native_schema_and_current_run() {
    let (session, mut parent, root, log, _) =
        compaction_test_session("structured-output", 128_000, 8_192);
    parent.cap_structured = true;
    let profile = crate::agent::builtin_profiles()
        .into_iter()
        .find(|profile| profile.name == "general-purpose")
        .expect("general profile");
    let mut prep = crate::agent::prepare_fresh_child(
        &crate::agent::ChildPrepCtx {
            cwd: &session.agent.cwd,
            data_dir: &session.data_dir,
            git_snapshot: None,
            skills_prompt: "",
            agents_prompt: "",
            runtime: None,
            parent_config: &parent,
            session_id: &session.agent.id,
            parent_agent_id: &session.agent.id,
            parent_delegation_depth: 0,
            max_delegation_depth: 1,
            max_agents_per_session: 64,
            allowed_subagents: None,
            child_cwd: None,
            worktree: false,
            parent_permissions: session.permissions,
            parent_history: &[],
            profiles_summary: "",
            state: &session.state,
            mcp: None,
            plugin_tools: session.plugin_tools.clone(),
        },
        profile,
        parent.clone(),
        pig_provider::mock::STRUCTURED_OUTPUT_TRIGGER,
        "structured child",
        &mut 0,
    )
    .expect("structured child preparation");
    prep.policy.output_schema = Some(serde_json::json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"]
    }));
    let mut child = Session::new_subagent(
        prep,
        &session,
        ProgressSink::Foreground {
            parent_item_id: "parent".into(),
        },
        SubagentMode::Foreground,
    );
    let writer = child
        .subagent
        .as_mut()
        .expect("child")
        .take_writer()
        .expect("writer");
    let (tx, _rx) = async_channel::unbounded();
    let mut life = AgentRunSupervisor::new(
        &child.state,
        &test_pending(),
        child.event_session_id(),
        tx.clone(),
        &child,
        writer,
        None,
    );
    let result = child
        .run_subagent_task(&tx, &CancellationToken::new())
        .await;
    assert_eq!(result.status, crate::session::AgentRunStatus::Completed);
    assert_eq!(
        result
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.structured.as_ref()),
        Some(&serde_json::json!({"answer": "ok"}))
    );
    let partial = child
        .state
        .agents
        .partial(&child.agent.id)
        .expect("partial output");
    assert_eq!(partial.run_id, result.run_id.as_deref().expect("run id"));
    assert_eq!(partial.text, "{\"answer\":\"ok\"}");
    let requests = log.lock().expect("request log").join("\n");
    assert!(requests.contains("response_format"), "{requests}");
    assert!(requests.contains("agent_result"), "{requests}");
    life.settle(&result);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn catalog_preserves_root_and_child_run_identities_across_resume() {
    let (mut session, parent_model, root, _log, runtime) =
        compaction_test_session("catalog", 128_000, 8_192);
    session.begin_root_run(pig_protocol::TurnKind::Regular);
    session.run_state.turns = 2;
    session.run_state.input = 7;
    session.run_state.output = 3;
    let first_root = session.root_result(crate::session::AgentRunStatus::Completed);
    assert_eq!(
        first_root.agent_id.as_deref(),
        Some(session.agent.id.as_str())
    );
    assert_eq!(first_root.turns, 2);
    assert_eq!(first_root.usage, (7, 0, 3));
    let first_root_id = first_root.run_id.clone().expect("root run id");
    session.begin_root_run(pig_protocol::TurnKind::Regular);
    let second_root = session.root_result(crate::session::AgentRunStatus::Completed);
    assert_ne!(second_root.run_id.as_deref(), Some(first_root_id.as_str()));
    session.begin_root_run(pig_protocol::TurnKind::Notification);
    let notification = session.root_result(crate::session::AgentRunStatus::Completed);
    assert_eq!(notification.turn_kind, pig_protocol::TurnKind::Notification);
    let notification_run_id = notification.run_id.clone();

    let profile = crate::agent::AgentProfile {
        name: "explore".into(),
        description: "catalog test".into(),
        tools: None,
        subagents: None,
        model: None,
        thought_level: None,
        max_turns: None,
        inject_agents_md: true,
        system_prompt: "test child".into(),
        source: crate::agent::AgentSource::BuiltIn,
    };
    let prep = crate::agent::prepare_fresh_child(
        &crate::agent::ChildPrepCtx {
            cwd: &session.agent.cwd,
            data_dir: &session.data_dir,
            git_snapshot: None,
            skills_prompt: "",
            agents_prompt: "",
            runtime: Some(runtime.as_ref()),
            parent_config: &parent_model,
            session_id: &session.agent.id,
            parent_agent_id: &session.agent.id,
            parent_delegation_depth: 0,
            max_delegation_depth: 1,
            max_agents_per_session: 64,
            allowed_subagents: None,
            child_cwd: None,
            worktree: false,
            parent_permissions: session.permissions,
            parent_history: &[],
            profiles_summary: "",
            state: &session.state,
            mcp: None,
            plugin_tools: session.plugin_tools.clone(),
        },
        profile.clone(),
        parent_model.clone(),
        "Finish the catalog test",
        "catalog child",
        &mut session.agent_seq,
    )
    .expect("fresh child");
    let agent_id = prep.agent_id.clone();
    let first_run_id = prep.run_id.clone();
    let task_id = crate::task::register_agent_run_queued(
        &session.state,
        format!("Subagent explore: {}", prep.description),
        CancellationToken::new(),
        prep.run_metadata(),
        true,
    );
    let mut child = Session::new_subagent(
        prep,
        &session,
        ProgressSink::Background {
            task_id: task_id.clone(),
        },
        SubagentMode::Background,
    );
    let writer = child
        .subagent
        .as_mut()
        .expect("child")
        .take_writer()
        .expect("writer");
    let (tx, rx) = async_channel::unbounded();
    let mut life = AgentRunSupervisor::new(
        &session.state,
        &test_pending(),
        &session.agent.id,
        tx.clone(),
        &child,
        writer,
        Some(task_id),
    );
    let first = child
        .run_subagent_task(&tx, &CancellationToken::new())
        .await;
    assert_eq!(first.status, crate::session::AgentRunStatus::Completed);
    assert_eq!(first.agent_id.as_deref(), Some(agent_id.as_str()));
    assert_eq!(first.run_id.as_deref(), Some(first_run_id.as_str()));
    assert!(
        first
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.path.as_ref())
            .is_some()
    );
    life.settle(&first);

    let resume = crate::agent::prepare_resume_child(
        &crate::agent::ChildPrepCtx {
            cwd: &session.agent.cwd,
            data_dir: &session.data_dir,
            git_snapshot: None,
            skills_prompt: "",
            agents_prompt: "",
            runtime: Some(runtime.as_ref()),
            parent_config: &parent_model,
            session_id: &session.agent.id,
            parent_agent_id: &session.agent.id,
            parent_delegation_depth: 0,
            max_delegation_depth: 1,
            max_agents_per_session: 64,
            allowed_subagents: None,
            child_cwd: None,
            worktree: false,
            parent_permissions: session.permissions,
            parent_history: &[],
            profiles_summary: "",
            state: &session.state,
            mcp: None,
            plugin_tools: session.plugin_tools.clone(),
        },
        &[profile],
        &agent_id,
        "Finish the resumed catalog test",
        "catalog child resumed",
    )
    .expect("resume child");
    assert_eq!(resume.agent_id, agent_id);
    assert_ne!(resume.run_id, first_run_id);
    let second_run_id = resume.run_id.clone();
    let task_id = crate::task::register_agent_run_queued(
        &session.state,
        "Subagent explore: catalog child resumed".into(),
        CancellationToken::new(),
        resume.run_metadata(),
        true,
    );
    let mut child = Session::new_subagent(
        resume,
        &session,
        ProgressSink::Background {
            task_id: task_id.clone(),
        },
        SubagentMode::Background,
    );
    let writer = child
        .subagent
        .as_mut()
        .expect("child")
        .take_writer()
        .expect("writer");
    let mut life = AgentRunSupervisor::new(
        &session.state,
        &test_pending(),
        &session.agent.id,
        tx.clone(),
        &child,
        writer,
        Some(task_id),
    );
    let second = child
        .run_subagent_task(&tx, &CancellationToken::new())
        .await;
    assert_eq!(second.status, crate::session::AgentRunStatus::Completed);
    life.settle(&second);
    assert!(
        std::iter::from_fn(|| rx.try_recv().ok()).any(|event| matches!(
            event,
            Event::ConversationStatus {
                run_id: Some(id),
                ..
            } if id == second_run_id
        ))
    );

    let catalog = session.agent_catalog();
    assert_eq!(catalog.len(), 3);
    assert_eq!(catalog[0].agent_id, session.agent.id);
    assert_eq!(catalog[0].run_id, notification_run_id);
    assert_eq!(
        catalog[0].turn_kind,
        Some(pig_protocol::TurnKind::Notification)
    );
    assert_eq!(catalog[0].profile, "main");
    assert!(catalog[0].record_path.is_some());
    for run_id in [first_run_id, second_run_id] {
        let entry = catalog
            .iter()
            .find(|entry| entry.run_id.as_deref() == Some(run_id.as_str()))
            .expect("child catalog entry");
        assert_eq!(entry.agent_id, agent_id);
        assert_eq!(entry.parent_id.as_deref(), Some(session.agent.id.as_str()));
        assert_eq!(entry.depth, 1);
        assert_eq!(entry.profile, "explore");
        assert_eq!(
            entry.status,
            Some(crate::session::AgentRunStatus::Completed)
        );
        assert!(entry.turns > 0);
        assert!(entry.record_path.is_some());
        assert!(entry.result_path.is_some());
    }
    let _ = std::fs::remove_dir_all(root);
}
