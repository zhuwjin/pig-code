use super::*;

use crate::config::RuntimeConfig;

use super::lifecycle::*;
use super::persist::*;
use super::state::*;

mod compaction;
mod compaction_pause;
mod drive;
mod swarm;

fn test_model() -> ResolvedModel {
    ResolvedModel {
        base_url: "http://localhost".into(),
        api_key: "test".into(),
        model: "test-model".into(),
        context_window: 1000,
        max_output_tokens: 100,
        api_format: pig_protocol::ApiFormat::OpenAiChat,
        reasoning_params: None,
        cap_structured: false,
        cap_strict_tools: false,
        cap_web_search: false,
        web_search_tool: None,
        input_image: false,
        input_pdf: false,
        anthropic_cache_control: false,
        replay_native_search: false,
        prompt_cache_key: None,
        provider_name: "test".into(),
        tool_settings: Default::default(),
    }
}

fn test_drive(agent_id: &str) -> SubagentDrive {
    let jsonl = std::env::temp_dir().join(format!(
        "pig-managed-{agent_id}-{}-{}.jsonl",
        std::process::id(),
        crate::rollout::now_secs()
    ));
    SubagentDrive {
        agent_id: agent_id.to_string(),
        cwd: std::env::temp_dir(),
        profile: crate::agent::AgentProfile {
            name: "explore".into(),
            description: "test child".into(),
            tools: None,
            model: None,
            thought_level: None,
            max_turns: None,
            inject_agents_md: true,
            system_prompt: "test".into(),
            source: crate::agent::AgentSource::BuiltIn,
        },
        child_config: test_model(),
        tools: vec![],
        schemas: vec![],
        history: vec![],
        jsonl,
        max_turns: 1,
        description: "managed child".into(),
        mcp: None,
        plugin_tools: crate::plugins::PluginToolset::new(
            &crate::plugins::PluginRegistry::shared(std::env::temp_dir()),
            &crate::config::WorkspaceTrust::untrusted(&std::env::temp_dir()),
            None,
            "s-test",
        ),
        plugin_tool_filter: Default::default(),
        mcp_inherits_all: false,
        trusted: true,
        persist_failure: None,
        writer: None,
    }
}

fn test_pending() -> PendingApprovals {
    Arc::new(Mutex::new(HashMap::new()))
}

fn test_lifecycle(
    state: &crate::task::SessionToolState,
    pending: &PendingApprovals,
    drive: &SubagentDrive,
    task_id: Option<String>,
) -> ChildLifecycle {
    let writer = state
        .try_claim_agent_writer(&drive.agent_id)
        .expect("test writer claim");
    let progress = match &task_id {
        Some(task_id) => ProgressSink::Background {
            task_id: task_id.clone(),
        },
        None => ProgressSink::Foreground {
            parent_item_id: "parent-item".into(),
        },
    };
    ChildLifecycle::new(
        state,
        pending,
        "s-test",
        async_channel::unbounded().0,
        drive,
        writer,
        &progress,
        task_id,
    )
}

fn compaction_test_session(
    name: &str,
    context_window: u64,
    max_output_tokens: u64,
) -> (
    Session,
    ResolvedModel,
    PathBuf,
    pig_provider::mock::RequestLog,
    Arc<RuntimeConfig>,
) {
    compaction_test_session_with_format(
        name,
        context_window,
        max_output_tokens,
        pig_protocol::ApiFormat::OpenAiChat,
    )
}

fn compaction_test_session_with_format(
    name: &str,
    context_window: u64,
    max_output_tokens: u64,
    format: pig_protocol::ApiFormat,
) -> (
    Session,
    ResolvedModel,
    PathBuf,
    pig_provider::mock::RequestLog,
    Arc<RuntimeConfig>,
) {
    use pig_protocol::{ModelConfig, ProviderConfig, SessionMeta};

    let (port, log) = pig_provider::mock::start_mock_server_with_log();
    let root = std::env::temp_dir().join(format!(
        "pig-core-child-compact-{name}-{}-{}",
        std::process::id(),
        crate::rollout::now_secs()
    ));
    let cwd = root.join("ws");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&cwd).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data");
    std::fs::write(
        cwd.join(pig_provider::mock::MOCK_FILE_NAME),
        pig_provider::mock::MOCK_FILE_CONTENT,
    )
    .expect("mock file");
    let config = pig_protocol::AppConfig {
        default_provider: "mock".into(),
        default_model: "mock-model".into(),
        providers: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock Provider".into(),
            base_url: format!("http://127.0.0.1:{port}/v1"),
            requires_api_key: true,
            api_format: format,
            enabled: true,
            models: vec![ModelConfig::new(
                "mock-model",
                context_window,
                max_output_tokens,
            )],
            icon: None,
            key_url: None,
        }],
        ..Default::default()
    };
    let meta = SessionMeta {
        id: format!("s-child-compact-{name}"),
        title: name.into(),
        cwd: cwd.clone(),
        created_at: crate::rollout::now_secs(),
        updated_at: crate::rollout::now_secs(),
        pinned: false,
        archived: false,
        title_custom: false,
        provider_id: None,
        model_id: None,
        reasoning_level: None,
        permissions: pig_protocol::PermissionSettings::default(),
        plan_enabled: false,
    };
    let store = Arc::new(Mutex::new(
        crate::store::Store::open(&data_dir).expect("store"),
    ));
    let (task_tx, _task_rx) = tokio::sync::mpsc::unbounded_channel();
    let runtime = Arc::new(RuntimeConfig::from_snapshot(
        config,
        &cwd,
        std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
    ));
    let parent = runtime.resolve_model(None, None).expect("parent model");
    let mut session = Session::create(
        meta,
        test_pending(),
        Arc::new(Mutex::new(HashMap::new())),
        store,
        &data_dir.join("sessions"),
        data_dir.clone(),
        task_tx,
        &crate::plugins::PluginRegistry::shared(data_dir),
        Some(runtime.clone()),
    )
    .expect("session");
    session.set_permissions(pig_protocol::ExecMode::Auto.permissions());
    (session, parent, root, log, runtime)
}

/// A long single-task child context: several complete user segments and a
/// complete assistant/tool round in the middle. The optional markers make
/// a request typed-overflow (mock trigger), stall the summary body, or
/// fail the summary sidecar.
fn long_child_messages(
    system_trigger: Option<&str>,
    round_trigger: Option<&str>,
    round_extra: Option<&str>,
) -> Vec<ChatMsg> {
    let system = match system_trigger {
        Some(trigger) => format!("You are a subagent. {trigger}"),
        None => "You are a subagent.".to_string(),
    };
    let mut history = vec![
        ChatMsg::system(system),
        ChatMsg::user(format!("old topic A {}", "a".repeat(8_000))),
        ChatMsg::assistant("A done".into(), vec![], None),
        ChatMsg::user(format!("old topic B {}", "b".repeat(6_000))),
    ];
    let call = ToolCall {
        id: "call_old_round".into(),
        name: "Read".into(),
        arguments: "{\"path\":\"old.txt\"}".into(),
    };
    history.push(ChatMsg::assistant(String::new(), vec![call], None));
    let mut round = round_trigger.map(str::to_string).unwrap_or_default();
    if let Some(extra) = round_extra {
        round.push_str(extra);
        round.push('\n');
    }
    round.push_str(&"b".repeat(6_000));
    history.push(ChatMsg::tool_result("call_old_round", round));
    history.push(ChatMsg::assistant("B done".into(), vec![], None));
    history.push(ChatMsg::user(format!("old topic C {}", "c".repeat(20_000))));
    history.push(ChatMsg::assistant(
        format!("C done {}", "c".repeat(20_000)),
        vec![],
        None,
    ));
    history
}

fn seed_child_context(session: &Session, agent_id: &str, history: &mut [ChatMsg]) -> PathBuf {
    let agents_dir =
        crate::agent::prepared_agent_dir(&session.data_dir.join("sessions"), &session.id)
            .expect("agents dir");
    let jsonl = agents_dir.join(format!("{agent_id}.jsonl"));
    persist_agent_line(
        &jsonl,
        &serde_json::json!({
            "type": "meta",
            "agent_id": agent_id,
            "profile": "explore",
            "description": "long child compaction fixture",
            "model": "mock-model",
            "provider": "Mock Provider",
            "created_at": 1,
        }),
    );
    for message in history {
        persist_agent_msg(&jsonl, message);
    }
    jsonl
}

fn child_resume_call(call_id: &str, agent_id: &str, tag: &str, background: bool) -> ToolCall {
    ToolCall {
        id: call_id.into(),
        name: "Agent".into(),
        arguments: serde_json::json!({
            "description": format!("resume long child {tag}"),
            "prompt": format!("SUBAGENT_CHILD:RESUMED {tag}"),
            "resume": agent_id,
            "run_in_background": background,
        })
        .to_string(),
    }
}

fn compact_lines(jsonl: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(jsonl)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record| record["type"] == "compact")
        .collect()
}

fn child_stream_requests(
    log: &pig_provider::mock::RequestLog,
    marker: &str,
) -> Vec<serde_json::Value> {
    log.lock()
        .expect("request log")
        .iter()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .filter(|request| {
            request["stream"] == true
                && request.to_string().contains(marker)
                && request["messages"][0]["content"]
                    .as_str()
                    .is_some_and(|system| !system.contains("Pig Code"))
        })
        .collect()
}

/// [`child_stream_requests`] for a specific wire shape: the parent system
/// prompt is `messages[0].content` on Chat, `instructions` on Responses and
/// `system` on Anthropic.
fn child_stream_requests_for_format(
    log: &pig_provider::mock::RequestLog,
    marker: &str,
    format: pig_protocol::ApiFormat,
) -> Vec<serde_json::Value> {
    let system_text = |request: &serde_json::Value| -> String {
        match format {
            pig_protocol::ApiFormat::OpenAiChat => request["messages"][0]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            pig_protocol::ApiFormat::OpenAiResponses => request["instructions"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            pig_protocol::ApiFormat::AnthropicMessages => match &request["system"] {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Array(items) => items
                    .iter()
                    .filter_map(|item| item["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            },
        }
    };
    log.lock()
        .expect("request log")
        .iter()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .filter(|request| {
            request["stream"] == true
                && request.to_string().contains(marker)
                && !system_text(request).contains("Pig Code")
        })
        .collect()
}

fn summary_requests(log: &pig_provider::mock::RequestLog) -> Vec<String> {
    log.lock()
        .expect("request log")
        .iter()
        .filter(|body| body.contains("[COMPACTION]") && body.contains("\"stream\":false"))
        .cloned()
        .collect()
}

fn child_task_statuses(session: &Session) -> Vec<(String, pig_protocol::TaskStatus)> {
    session
        .state
        .tasks
        .lock()
        .expect("task registry lock")
        .iter()
        .map(|task| (task.id.clone(), task.status))
        .collect()
}

async fn wait_for_background_child(session: &Session) {
    let deadline = Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let tasks = child_task_statuses(session);
        let settled = !tasks.is_empty()
            && tasks.iter().all(|task| {
                !matches!(
                    task.1,
                    pig_protocol::TaskStatus::Queued | pig_protocol::TaskStatus::Running
                )
            });
        if settled {
            return;
        }
        assert!(Instant::now() < deadline, "background child did not settle");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[test]
fn child_local_state_has_its_own_todo_handle() {
    let parent = crate::task::SessionToolState::for_test();
    parent
        .todos
        .lock()
        .expect("parent todos")
        .push(pig_protocol::TodoItem {
            content: "parent item".into(),
            status: pig_protocol::TodoStatus::Pending,
        });
    let child = child_local_state(&parent, "agent-test");
    assert!(
        child.todos.lock().expect("child todos").is_empty(),
        "a child starts from its own empty list, never the parent snapshot"
    );
    assert_eq!(
        child.owner_agent_id.as_deref(),
        Some("agent-test"),
        "tasks the child spawns are owned by that child"
    );
    child
        .todos
        .lock()
        .expect("child todos")
        .push(pig_protocol::TodoItem {
            content: "child item".into(),
            status: pig_protocol::TodoStatus::InProgress,
        });
    assert_eq!(
        parent.todos.lock().expect("parent todos").len(),
        1,
        "a child write must not clobber the parent list"
    );
    assert!(
        !std::sync::Arc::ptr_eq(&parent.todos, &child.todos),
        "each child owns a distinct TodoHandle"
    );
}

/// Child gated execution writes the local list but never persists to the
/// session's todos table or emits the root TodoListChanged event.
#[tokio::test]
async fn child_todo_write_stays_out_of_root_state_and_events() {
    let root = std::env::temp_dir().join(format!(
        "pig-child-todo-{}-{}",
        std::process::id(),
        crate::rollout::now_secs()
    ));
    std::fs::create_dir_all(&root).expect("temp workspace");
    let store = Arc::new(Mutex::new(crate::store::Store::open(&root).expect("store")));
    let parent = crate::task::SessionToolState::for_test();
    let child_state = child_local_state(&parent, "agent-test");
    let tool = crate::tool::all()
        .into_iter()
        .find(|tool| tool.name() == "TodoList")
        .expect("TodoList tool");
    let mut tracker = crate::tool::ChangeTracker::default();
    let pending = test_pending();
    let mut rules = crate::permissions::PermissionRules::default();
    let mut always_allowed = HashSet::new();
    let hooks = crate::hooks::HookSet::load(&root, None);
    let (tx, rx) = async_channel::unbounded();
    let cancel = CancellationToken::new();
    let call = pig_provider::ToolCall {
        id: "todo-local".into(),
        name: "TodoList".into(),
        arguments: serde_json::json!({
            "todos": [{"content": "child item", "status": "pending"}]
        })
        .to_string(),
    };
    let mut gate = GateCtx {
        cwd: &root,
        permissions: pig_protocol::PermissionSettings::default(),
        media: crate::tool::MediaCapabilities::new(true, false),
        tracker: &mut tracker,
        state: &child_state,
        owns_todo_state: false,
        pending: &pending,
        permission_rules: &mut rules,
        always_allowed: &mut always_allowed,
        session_id: "s-child-todo",
        plan_enabled: false,
        store: &store,
        extra_tools: &[],
        hooks: &hooks,
        history: &[],
    };
    let outcome = exec_tool_gated_ctx(
        &mut gate,
        &call,
        Some(tool.as_ref()),
        "item-todo",
        "turn",
        &tx,
        &cancel,
    )
    .await;
    assert!(
        matches!(outcome, GatedToolOutcome::Executed { .. }),
        "the TodoList call executes in the child gate"
    );
    assert_eq!(
        child_state.todos.lock().expect("child todos").len(),
        1,
        "the child-local list receives the write"
    );
    assert!(
        parent.todos.lock().expect("parent todos").is_empty(),
        "the parent list is untouched"
    );
    assert!(
        store
            .lock()
            .expect("store")
            .get_todos("s-child-todo")
            .is_none(),
        "a child never persists the session todo snapshot"
    );
    assert!(
        rx.try_recv().is_err(),
        "a child never emits the root TodoListChanged event"
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn swarm_test_session(name: &str) -> (Session, ResolvedModel, PathBuf) {
    use pig_protocol::{ApiFormat, ModelConfig, ProviderConfig, SessionMeta};

    let port = pig_provider::mock::start_mock_server();
    let root = std::env::temp_dir().join(format!(
        "pig-core-swarm-{name}-{}-{}",
        std::process::id(),
        crate::rollout::now_secs()
    ));
    let cwd = root.join("ws");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&cwd).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data");
    std::fs::write(
        cwd.join(pig_provider::mock::MOCK_FILE_NAME),
        pig_provider::mock::MOCK_FILE_CONTENT,
    )
    .expect("mock file");

    let config = pig_protocol::AppConfig {
        default_provider: "mock".into(),
        default_model: "mock-model".into(),
        providers: vec![ProviderConfig {
            id: "mock".into(),
            name: "Mock Provider".into(),
            base_url: format!("http://127.0.0.1:{port}/v1"),
            requires_api_key: true,
            api_format: ApiFormat::OpenAiChat,
            enabled: true,
            models: vec![ModelConfig::new("mock-model", 128_000, 8_192)],
            icon: None,
            key_url: None,
        }],
        ..Default::default()
    };
    let meta = SessionMeta {
        id: format!("s-{name}"),
        title: name.into(),
        cwd: cwd.clone(),
        created_at: crate::rollout::now_secs(),
        updated_at: crate::rollout::now_secs(),
        pinned: false,
        archived: false,
        title_custom: false,
        provider_id: None,
        model_id: None,
        reasoning_level: None,
        permissions: pig_protocol::PermissionSettings::default(),
        plan_enabled: false,
    };
    let store = Arc::new(Mutex::new(
        crate::store::Store::open(&data_dir).expect("store"),
    ));
    let (task_tx, _task_rx) = tokio::sync::mpsc::unbounded_channel();
    let runtime = Arc::new(RuntimeConfig::from_snapshot(
        config,
        &cwd,
        std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
    ));
    let parent = runtime.resolve_model(None, None).expect("parent model");
    let mut session = Session::create(
        meta,
        test_pending(),
        Arc::new(Mutex::new(HashMap::new())),
        store,
        &data_dir.join("sessions"),
        data_dir.clone(),
        task_tx,
        &crate::plugins::PluginRegistry::shared(data_dir),
        Some(runtime.clone()),
    )
    .expect("session");
    session.set_permissions(pig_protocol::ExecMode::Auto.permissions());
    (session, parent, root)
}
