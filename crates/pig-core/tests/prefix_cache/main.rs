//! Restart prefix-cache regression coverage using real Op/Event traffic and
//! logged HTTP requests. Every old history item must survive a cold resume
//! byte-for-byte; only the new turn may append to the model-visible tail.

#[path = "../common/mod.rs"]
mod common;
mod split_turn;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pig_core::AgentHandle;
use pig_protocol::{Event, ExecMode, MessageDelivery, Op};
use pig_provider::mock;
use serde_json::Value;

const ORIGINAL_AGENTS: &str = "PREFIX_ORIGINAL_AGENTS_RULE";
const UPDATED_AGENTS: &str = "PREFIX_UPDATED_AGENTS_RULE";
const ORIGINAL_PROFILE: &str = "PREFIX_ORIGINAL_PROFILE_DESCRIPTION";
const UPDATED_PROFILE: &str = "PREFIX_UPDATED_PROFILE_DESCRIPTION";
const ORIGINAL_ATTACHMENT: &str = "original attachment";
const UPDATED_ATTACHMENT: &str = "updated attachment with a different byte length";
const COMPACTED_UNIQUE: &str = "PREFIX_COMPACTED_UNIQUE_TEXT_7D3A";
const PARALLEL_IDS: [&str; 2] = ["call_prefix_parallel_1", "call_prefix_parallel_2"];

type RequestLog = Arc<Mutex<Vec<String>>>;

#[derive(Clone, Copy, Debug)]
enum Format {
    Chat,
    Responses,
    Anthropic,
}

impl Format {
    fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
            Self::Anthropic => "anthropic",
        }
    }

    fn config_name(self) -> &'static str {
        match self {
            Self::Chat => "OpenAiChat",
            Self::Responses => "OpenAiResponses",
            Self::Anthropic => "AnthropicMessages",
        }
    }

    fn history_key(self) -> &'static str {
        match self {
            Self::Chat | Self::Anthropic => "messages",
            Self::Responses => "input",
        }
    }
}

static SANDBOX_CONFIG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn lock_sandbox_config() -> tokio::sync::MutexGuard<'static, ()> {
    loop {
        match SANDBOX_CONFIG_LOCK.try_lock() {
            Ok(guard) => return guard,
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

struct Fixture {
    config_path: PathBuf,
    workspace: PathBuf,
    data_dir: PathBuf,
    base_url: String,
    log: RequestLog,
    // Sandbox settings are process-global; isolated configs must not overwrite each other.
    _config_lock: tokio::sync::MutexGuard<'static, ()>,
}

impl Fixture {
    fn new(label: &str, format: Format, input_image: bool) -> Self {
        let config_lock = lock_sandbox_config();
        let (port, log) = mock::start_mock_server_with_log();
        let root = std::env::temp_dir().join(format!(
            "pig-core-prefix-cache-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join(mock::MOCK_FILE_NAME),
            mock::MOCK_FILE_CONTENT,
        )
        .unwrap();
        let config_path = root.join("config.toml");
        let input_image = if input_image {
            "input_image = true\n"
        } else {
            ""
        };
        std::fs::write(
            &config_path,
            format!(
                r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock Provider"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "{}"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
{input_image}"#,
                format.config_name()
            ),
        )
        .unwrap();
        Self {
            config_path,
            workspace,
            data_dir: root.join("data"),
            base_url: format!("http://127.0.0.1:{port}/v1"),
            log,
            _config_lock: config_lock,
        }
    }

    fn spawn(&self) -> AgentHandle {
        pig_core::spawn_agent_with_data_dir(
            Some(self.config_path.clone()),
            self.workspace.clone(),
            self.data_dir.clone(),
        )
    }
}

async fn send_with_files(
    agent: &AgentHandle,
    events: &async_channel::Receiver<Event>,
    session_id: &str,
    text: &str,
    files: Vec<String>,
) -> Vec<Event> {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: text.to_string(),
            files,
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await
}

async fn send_and_complete(
    agent: &AgentHandle,
    events: &async_channel::Receiver<Event>,
    session_id: &str,
    text: &str,
) -> Vec<Event> {
    send_with_files(agent, events, session_id, text, vec![]).await
}

async fn shutdown(agent: AgentHandle) {
    agent.shutdown();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !agent.ops.is_closed() {
        assert!(Instant::now() < deadline, "agent did not shut down");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn open_replayed(agent: &AgentHandle, session_id: &str) {
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.to_string(),
        })
        .await
        .unwrap();
    let replay = common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::TurnComplete {
                duration_ms: 0,
                stats: None,
                finish_reason: None,
                ..
            }
        )
    })
    .await;
    assert!(
        replay
            .iter()
            .any(|event| matches!(event, Event::SessionConfigured { .. })),
        "cold open did not configure the session: {replay:#?}"
    );
    assert!(
        replay
            .iter()
            .any(|event| matches!(event, Event::UserMessage { .. })),
        "cold open did not replay durable history: {replay:#?}"
    );
}

fn is_main_request(request: &Value, format: Format) -> bool {
    if request.get("stream").and_then(Value::as_bool) != Some(true)
        || !request
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
    {
        return false;
    }
    match format {
        Format::Chat => {
            request["messages"][0]["role"] == "system"
                && request["messages"][0]["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("Pig Code"))
        }
        Format::Responses => request["instructions"]
            .as_str()
            .is_some_and(|instructions| instructions.contains("Pig Code")),
        Format::Anthropic => request["system"].to_string().contains("Pig Code"),
    }
}

fn main_requests(log: &RequestLog, format: Format) -> Vec<Value> {
    log.lock()
        .expect("request log")
        .iter()
        .filter_map(|body| serde_json::from_str::<Value>(body).ok())
        .filter(|request| is_main_request(request, format))
        .collect()
}

fn prefix_violations(before: &Value, after: &Value, format: Format) -> Vec<String> {
    let mut violations = Vec::new();
    match format {
        Format::Chat => {}
        Format::Responses => {
            if before["instructions"] != after["instructions"] {
                violations.push("instructions changed".to_string());
            }
        }
        Format::Anthropic => {
            if before["system"] != after["system"] {
                violations.push("top-level system changed".to_string());
            }
        }
    }
    if before["tools"] != after["tools"] {
        violations.push("tools changed".to_string());
    }

    let key = format.history_key();
    let old_items = before[key].as_array().expect("old history array");
    let new_items = after[key].as_array().expect("new history array");
    if new_items.len() < old_items.len() {
        violations.push(format!(
            "{key} shrank from {} to {} items",
            old_items.len(),
            new_items.len()
        ));
    }
    for (index, old_item) in old_items.iter().enumerate() {
        match new_items.get(index) {
            Some(new_item) if new_item == old_item => {}
            Some(_) => violations.push(format!("{key}[{index}] changed")),
            None => violations.push(format!("{key}[{index}] is missing")),
        }
    }
    violations
}

fn assert_prefix_contract(before: &Value, after: &Value, format: Format, context: &str) {
    let violations = prefix_violations(before, after, format);
    assert!(
        violations.is_empty(),
        "{} {context}: restart prefix contract failed:\n{}",
        format.label(),
        violations.join("\n")
    );
}

fn appended_tail_text(before: &Value, after: &Value, format: Format) -> String {
    let key = format.history_key();
    let old_len = before[key].as_array().expect("old history array").len();
    after[key]
        .as_array()
        .expect("new history array")
        .get(old_len..)
        .expect("new history contains the old prefix")
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn profile(description: &str) -> String {
    format!("---\nname: cacheprobe\ndescription: {description}\n---\nProfile body.")
}

async fn assert_environment_restart(format: Format) {
    let fixture = Fixture::new(&format!("env-{}", format.label()), format, false);
    let agents_dir = fixture.workspace.join(".pigcode").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(fixture.workspace.join("AGENTS.md"), ORIGINAL_AGENTS).unwrap();
    std::fs::write(agents_dir.join("cacheprobe.md"), profile(ORIGINAL_PROFILE)).unwrap();
    std::fs::write(fixture.workspace.join("attached.txt"), ORIGINAL_ATTACHMENT).unwrap();
    // Project agent profiles are admitted only by the real workspace-trust
    // decision (the same list that admits project hooks).
    let canonical = fixture
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| fixture.workspace.clone())
        .display()
        .to_string()
        .replace('\\', "\\\\");
    let mut config = std::fs::read_to_string(&fixture.config_path).unwrap();
    config.push_str(&format!(
        "\n[hooks]\ntrusted_projects = [\"{canonical}\"]\n"
    ));
    std::fs::write(&fixture.config_path, config).unwrap();

    let agent = fixture.spawn();
    let session_id = common::new_session(&agent, fixture.workspace.clone()).await;
    send_with_files(
        &agent,
        &agent.events,
        &session_id,
        "ordinary tool turn before restart",
        vec!["attached.txt".to_string()],
    )
    .await;
    let requests = main_requests(&fixture.log, format);
    let previous_request_count = requests.len();
    let before = requests.last().expect("request before restart").clone();
    let before_text = before.to_string();
    assert!(before_text.contains(ORIGINAL_AGENTS), "{}", format.label());
    assert!(
        before_text.contains(ORIGINAL_PROFILE),
        "{}: custom profile missing from tools",
        format.label()
    );
    assert!(
        before_text.contains("Current execution mode: auto"),
        "{}: first user message lacks the permission reminder",
        format.label()
    );
    let original_pointer = format!(
        "[Referenced file attached.txt ({} B)]",
        ORIGINAL_ATTACHMENT.len()
    );
    assert!(
        before_text.contains(&original_pointer),
        "{}: first user message lacks its @file pointer",
        format.label()
    );
    shutdown(agent).await;

    std::fs::write(fixture.workspace.join("AGENTS.md"), UPDATED_AGENTS).unwrap();
    std::fs::write(agents_dir.join("cacheprobe.md"), profile(UPDATED_PROFILE)).unwrap();
    std::fs::write(fixture.workspace.join("attached.txt"), UPDATED_ATTACHMENT).unwrap();

    let agent = fixture.spawn();
    open_replayed(&agent, &session_id).await;
    send_and_complete(
        &agent,
        &agent.events,
        &session_id,
        "new turn after environment restart",
    )
    .await;
    let requests = main_requests(&fixture.log, format);
    assert!(requests.len() > previous_request_count);
    let after = requests.last().expect("request after restart");
    assert_prefix_contract(&before, after, format, "environment changes");

    let tail = appended_tail_text(&before, after, format);
    assert!(
        tail.contains("AGENTS.md content has been updated") && tail.contains(UPDATED_AGENTS),
        "{}: AGENTS.md changes must ride the appended reminder, tail: {tail}",
        format.label()
    );
    let updated_pointer = format!(
        "[Referenced file attached.txt ({} B)]",
        UPDATED_ATTACHMENT.len()
    );
    assert!(
        !after.to_string().contains(&updated_pointer),
        "{}: restart recomputed an old @file size",
        format.label()
    );
    shutdown(agent).await;
}

async fn assert_compact_restart(format: Format) {
    let fixture = Fixture::new(&format!("compact-{}", format.label()), format, false);
    let agent = fixture.spawn();
    let session_id = common::new_session(&agent, fixture.workspace.clone()).await;
    send_and_complete(
        &agent,
        &agent.events,
        &session_id,
        &format!("{COMPACTED_UNIQUE} read the mock file"),
    )
    .await;
    send_and_complete(
        &agent,
        &agent.events,
        &session_id,
        "second history round before compact",
    )
    .await;
    agent
        .ops
        .send(Op::Compact {
            session_id: session_id.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let compacted = common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(event, Event::ContextCompacted { .. })
    })
    .await;
    assert!(
        compacted.iter().any(|event| matches!(
            event,
            Event::ContextCompacted {
                omitted,
                automatic: false,
                summary: Some(summary),
                ..
            } if *omitted > 0 && summary.contains(mock::SUMMARY_MARKER)
        )),
        "{}: expected a real model-summary compact: {compacted:#?}",
        format.label()
    );

    send_and_complete(
        &agent,
        &agent.events,
        &session_id,
        "post-compact turn before restart",
    )
    .await;
    let requests = main_requests(&fixture.log, format);
    let previous_request_count = requests.len();
    let before = requests
        .last()
        .expect("last post-compact request before restart")
        .clone();
    assert!(
        !before.to_string().contains(COMPACTED_UNIQUE),
        "{}: live compact did not remove the unique text",
        format.label()
    );
    shutdown(agent).await;

    let agent = fixture.spawn();
    open_replayed(&agent, &session_id).await;
    send_and_complete(
        &agent,
        &agent.events,
        &session_id,
        "new turn after compact restart",
    )
    .await;
    let requests = main_requests(&fixture.log, format);
    assert!(requests.len() > previous_request_count);
    let after = requests.last().expect("request after compact restart");
    let mut violations = prefix_violations(&before, after, format);
    if after.to_string().contains(COMPACTED_UNIQUE) {
        violations.push("compacted unique text resurrected".to_string());
    }
    assert!(
        violations.is_empty(),
        "{}: compact restart contract failed:\n{}",
        format.label(),
        violations.join("\n")
    );
    shutdown(agent).await;
}

fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(width, height)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

async fn assert_media_restart(format: Format) {
    let fixture = Fixture::new(&format!("media-{}", format.label()), format, true);
    std::fs::write(fixture.workspace.join("pic.png"), png_bytes(64, 48)).unwrap();
    let agent = fixture.spawn();
    let session_id = common::new_session(&agent, fixture.workspace.clone()).await;
    let events = send_and_complete(
        &agent,
        &agent.events,
        &session_id,
        &format!("{} read the image", mock::SCENARIO_MEDIA_TRIGGER),
    )
    .await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::ToolCallEnd {
                output,
                is_error: false,
                ..
            } if output.contains("Read image pic.png")
        )),
        "{}: ReadMediaFile did not complete: {events:#?}",
        format.label()
    );
    let requests = main_requests(&fixture.log, format);
    let previous_request_count = requests.len();
    let before = requests
        .last()
        .expect("media request before restart")
        .clone();
    let before_text = before.to_string();
    assert!(before_text.contains("Read image pic.png"));
    assert!(
        before_text.contains("iVBORw0KGgo"),
        "{}: old media request has no PNG payload",
        format.label()
    );
    shutdown(agent).await;

    let agent = fixture.spawn();
    open_replayed(&agent, &session_id).await;
    send_and_complete(
        &agent,
        &agent.events,
        &session_id,
        "new turn after media restart",
    )
    .await;
    let requests = main_requests(&fixture.log, format);
    assert!(requests.len() > previous_request_count);
    let after = requests.last().expect("media request after restart");
    assert_prefix_contract(&before, after, format, "ReadMediaFile image and text");
    shutdown(agent).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_restart_preserves_prefix_across_environment_changes() {
    assert_environment_restart(Format::Chat).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_restart_preserves_prefix_across_environment_changes() {
    assert_environment_restart(Format::Responses).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_restart_preserves_prefix_across_environment_changes() {
    assert_environment_restart(Format::Anthropic).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_compact_restart_preserves_post_compact_prefix() {
    assert_compact_restart(Format::Chat).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_compact_restart_preserves_post_compact_prefix() {
    assert_compact_restart(Format::Responses).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_compact_restart_preserves_post_compact_prefix() {
    assert_compact_restart(Format::Anthropic).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_read_media_restart_preserves_image_and_text_prefix() {
    assert_media_restart(Format::Chat).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_read_media_restart_preserves_image_and_text_prefix() {
    assert_media_restart(Format::Responses).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_read_media_restart_preserves_image_and_text_prefix() {
    assert_media_restart(Format::Anthropic).await;
}

fn assert_parallel_shape(items: &[Value], format: Format) {
    match format {
        Format::Chat => {
            let assistants: Vec<_> = items
                .iter()
                .filter(|message| {
                    message["role"] == "assistant"
                        && message["tool_calls"]
                            .as_array()
                            .is_some_and(|calls| !calls.is_empty())
                })
                .collect();
            assert_eq!(assistants.len(), 1, "chat assistant grouping");
            let calls = assistants[0]["tool_calls"].as_array().unwrap();
            assert_eq!(calls.len(), 2);
            let ids: Vec<_> = calls
                .iter()
                .map(|call| call["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, PARALLEL_IDS);
            assert!(calls.iter().all(|call| call["function"]["name"] == "Read"));
            let result_ids: Vec<_> = items
                .iter()
                .filter(|message| message["role"] == "tool")
                .map(|message| message["tool_call_id"].as_str().unwrap())
                .collect();
            assert_eq!(result_ids, PARALLEL_IDS);
        }
        Format::Responses => {
            let calls: Vec<_> = items
                .iter()
                .enumerate()
                .filter(|(_, item)| item["type"] == "function_call")
                .collect();
            assert_eq!(calls.len(), 2, "responses assistant call group");
            assert_eq!(calls[0].0 + 1, calls[1].0, "calls must stay adjacent");
            let ids: Vec<_> = calls
                .iter()
                .map(|(_, call)| call["call_id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, PARALLEL_IDS);
            assert!(calls.iter().all(|(_, call)| call["name"] == "Read"));
            let result_ids: Vec<_> = items
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .map(|item| item["call_id"].as_str().unwrap())
                .collect();
            assert_eq!(result_ids, PARALLEL_IDS);
        }
        Format::Anthropic => {
            let assistants: Vec<_> = items
                .iter()
                .filter(|message| {
                    message["role"] == "assistant"
                        && message["content"].as_array().is_some_and(|blocks| {
                            blocks.iter().any(|block| block["type"] == "tool_use")
                        })
                })
                .collect();
            assert_eq!(assistants.len(), 1, "anthropic assistant grouping");
            let blocks = assistants[0]["content"].as_array().unwrap();
            let calls: Vec<_> = blocks
                .iter()
                .filter(|block| block["type"] == "tool_use")
                .collect();
            assert_eq!(calls.len(), 2);
            let ids: Vec<_> = calls
                .iter()
                .map(|call| call["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, PARALLEL_IDS);
            assert!(calls.iter().all(|call| call["name"] == "Read"));
            let result_ids: Vec<_> = items
                .iter()
                .filter(|message| message["role"] == "user")
                .filter_map(|message| message["content"].as_array())
                .flat_map(|blocks| blocks.iter())
                .filter(|block| block["type"] == "tool_result")
                .map(|block| block["tool_use_id"].as_str().unwrap())
                .collect();
            assert_eq!(result_ids, PARALLEL_IDS);
        }
    }
}

async fn assert_parallel_restart(format: Format) {
    let fixture = Fixture::new(&format!("parallel-{}", format.label()), format, false);
    let agent = fixture.spawn();
    let session_id = common::new_session(&agent, fixture.workspace.clone()).await;
    send_and_complete(
        &agent,
        &agent.events,
        &session_id,
        mock::PREFIX_PARALLEL_SCENARIO,
    )
    .await;
    let requests = main_requests(&fixture.log, format);
    let previous_request_count = requests.len();
    let before = requests
        .last()
        .expect("parallel-call request before restart")
        .clone();
    assert_parallel_shape(before[format.history_key()].as_array().unwrap(), format);
    shutdown(agent).await;

    let agent = fixture.spawn();
    open_replayed(&agent, &session_id).await;
    send_and_complete(
        &agent,
        &agent.events,
        &session_id,
        "new turn after parallel-call restart",
    )
    .await;
    let requests = main_requests(&fixture.log, format);
    assert!(requests.len() > previous_request_count);
    let after = requests
        .last()
        .expect("parallel-call request after restart");
    assert_prefix_contract(&before, after, format, "parallel tool calls");
    let old_len = before[format.history_key()].as_array().unwrap().len();
    assert_parallel_shape(
        &after[format.history_key()].as_array().unwrap()[..old_len],
        format,
    );
    shutdown(agent).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_parallel_calls_keep_ids_order_and_single_assistant_group_after_restart() {
    assert_parallel_restart(Format::Chat).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_parallel_calls_keep_ids_order_and_single_assistant_group_after_restart() {
    assert_parallel_restart(Format::Responses).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_parallel_calls_keep_ids_order_and_single_assistant_group_after_restart() {
    assert_parallel_restart(Format::Anthropic).await;
}

fn plain_text(value: &Value) -> String {
    fn collect(value: &Value, out: &mut String) {
        match value {
            Value::String(text) => {
                out.push_str(text);
                out.push('\n');
            }
            Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
            Value::Object(items) => items.values().for_each(|item| collect(item, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    collect(value, &mut out);
    out
}

fn system_text(request: &Value, format: Format) -> String {
    match format {
        Format::Chat => plain_text(&request["messages"][0]["content"]),
        Format::Responses => plain_text(&request["instructions"]),
        Format::Anthropic => plain_text(&request["system"]),
    }
}

fn appended_tail_plain_text(before: &Value, after: &Value, format: Format) -> String {
    let key = format.history_key();
    let old_len = before[key].as_array().unwrap().len();
    after[key].as_array().unwrap()[old_len..]
        .iter()
        .map(plain_text)
        .collect()
}

fn latest_access_boundary(text: &str) -> Option<&str> {
    text.lines()
        .rev()
        .find(|line| line.contains("Current access boundary"))
}

fn set_memory_enabled(config_path: &std::path::Path, enabled: bool) {
    let mut config = std::fs::read_to_string(config_path).unwrap();
    if enabled {
        assert!(!config.contains("[memory]"));
        config.push_str("\n[memory]\nenabled = true\n");
    } else {
        let before = config.clone();
        config = config.replacen(
            "\n[memory]\nenabled = true\n",
            "\n[memory]\nenabled = false\n",
            1,
        );
        assert_ne!(config, before, "memory config must change");
    }
    std::fs::write(config_path, config).unwrap();
}

async fn assert_memory_disable_restart(format: Format) {
    let fixture = Fixture::new(&format!("memory-off-{}", format.label()), format, false);
    set_memory_enabled(&fixture.config_path, true);
    let memory_dir = pig_core::memory::workspace_memory_dir(&fixture.data_dir, &fixture.workspace);
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(
        memory_dir.join("MEMORY.md"),
        "# Memory\n\n- [Restart probe](restart.md) — PREFIX_RESTART_MEMORY_INDEX\n",
    )
    .unwrap();
    let agent = fixture.spawn();
    let session_id = common::new_session(&agent, fixture.workspace.clone()).await;
    send_and_complete(&agent, &agent.events, &session_id, "memory enabled").await;
    let requests = main_requests(&fixture.log, format);
    let previous_request_count = requests.len();
    let before = requests.last().expect("memory-enabled request").clone();
    let enabled_system = system_text(&before, format);
    assert!(enabled_system.contains("## Workspace memory"));
    assert!(enabled_system.contains("This directory already exists"));
    assert!(enabled_system.contains("PREFIX_RESTART_MEMORY_INDEX"));
    // `workspace_memory_dir` may carry the Windows verbatim prefix while the
    // prompt renders the dunce-canonicalized spelling.
    let memory_display = memory_dir.display().to_string();
    let memory_path = memory_display
        .strip_prefix(r"\\?\")
        .unwrap_or(memory_display.as_str());
    let before_text = plain_text(&before);
    let enabled_boundary = latest_access_boundary(&before_text).unwrap();
    assert!(
        enabled_boundary.contains(memory_path),
        "boundary={enabled_boundary}\nmemory_path={memory_path}"
    );
    shutdown(agent).await;

    set_memory_enabled(&fixture.config_path, false);
    let agent = fixture.spawn();
    open_replayed(&agent, &session_id).await;
    send_and_complete(&agent, &agent.events, &session_id, "memory disabled").await;
    let requests = main_requests(&fixture.log, format);
    assert!(requests.len() > previous_request_count);
    let after = requests.last().expect("memory-disabled request");
    let disabled_system = system_text(after, format);
    assert!(!disabled_system.contains("## Workspace memory"));
    assert!(!disabled_system.contains("This directory already exists"));
    assert!(!disabled_system.contains("PREFIX_RESTART_MEMORY_INDEX"));
    let tail = appended_tail_plain_text(&before, after, format);
    let disabled_boundary = latest_access_boundary(&tail).expect("new access boundary");
    assert!(
        !disabled_boundary.contains(memory_path),
        "{disabled_boundary}"
    );
    let workspace_display = fixture.workspace.display().to_string();
    let workspace_path = workspace_display
        .strip_prefix(r"\\?\")
        .unwrap_or(workspace_display.as_str());
    assert!(disabled_boundary.contains(workspace_path));
    assert_ne!(disabled_boundary, enabled_boundary);
    shutdown(agent).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_memory_disable_restart_refreshes_system_and_access_boundary() {
    assert_memory_disable_restart(Format::Chat).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_memory_disable_restart_refreshes_instructions_and_access_boundary() {
    assert_memory_disable_restart(Format::Responses).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_memory_disable_restart_refreshes_system_and_access_boundary() {
    assert_memory_disable_restart(Format::Anthropic).await;
}

async fn assert_deny_rule_restart(format: Format) {
    // A literal directory name that sorts inside access_line's 16-rule display cap.
    const DENY_RULE: &str = "!prefix-cache-secret";
    let fixture = Fixture::new(&format!("deny-restart-{}", format.label()), format, false);
    let agent = fixture.spawn();
    let session_id = common::new_session(&agent, fixture.workspace.clone()).await;
    send_and_complete(&agent, &agent.events, &session_id, "before deny rule").await;
    let requests = main_requests(&fixture.log, format);
    let mut previous_request_count = requests.len();
    let before = requests.last().expect("request before deny rule").clone();
    assert!(!plain_text(&before).contains(DENY_RULE));
    shutdown(agent).await;

    let mut config = std::fs::read_to_string(&fixture.config_path).unwrap();
    config.push_str(&format!(
        "\n[sandbox]\ndeny_read = [{}]\n",
        serde_json::to_string(DENY_RULE).unwrap()
    ));
    std::fs::write(&fixture.config_path, config).unwrap();
    let agent = fixture.spawn();
    open_replayed(&agent, &session_id).await;
    send_and_complete(&agent, &agent.events, &session_id, "after deny rule").await;
    let requests = main_requests(&fixture.log, format);
    assert!(requests.len() > previous_request_count);
    previous_request_count = requests.len();
    let changed = requests.last().expect("request after deny rule").clone();
    assert_prefix_contract(&before, &changed, format, "deny rule change");
    let tail = appended_tail_plain_text(&before, &changed, format);
    let boundary = latest_access_boundary(&tail).expect("changed access boundary");
    assert!(boundary.contains("Denied filesystem reads"), "{boundary}");
    assert!(boundary.contains(DENY_RULE), "{boundary}");

    send_and_complete(&agent, &agent.events, &session_id, "unchanged deny rule").await;
    let requests = main_requests(&fixture.log, format);
    assert!(requests.len() > previous_request_count);
    let unchanged = requests.last().expect("request with unchanged deny rule");
    assert_prefix_contract(&changed, unchanged, format, "unchanged deny rule");
    let tail = appended_tail_plain_text(&changed, unchanged, format);
    assert!(tail.contains("unchanged deny rule"));
    assert!(
        !tail.contains("Current access boundary"),
        "unchanged boundary must not repeat: {tail}"
    );
    shutdown(agent).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_deny_rule_restart_updates_boundary_once() {
    assert_deny_rule_restart(Format::Chat).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_deny_rule_restart_updates_boundary_once() {
    assert_deny_rule_restart(Format::Responses).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_deny_rule_restart_updates_boundary_once() {
    assert_deny_rule_restart(Format::Anthropic).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_skills_disable_restart_refreshes_system_prompt() {
    let fixture = Fixture::new("skills-disable-chat", Format::Chat, false);
    let skill_md = fixture.data_dir.join("skills/restart-probe/SKILL.md");
    std::fs::create_dir_all(skill_md.parent().unwrap()).unwrap();
    std::fs::write(
        &skill_md,
        "---\nname: restart-probe\ndescription: PREFIX_SKILL_STATE_DESCRIPTION\n---\nBody",
    )
    .unwrap();
    let agent = fixture.spawn();
    let session_id = common::new_session(&agent, fixture.workspace.clone()).await;
    send_and_complete(&agent, &agent.events, &session_id, "skill enabled").await;
    let requests = main_requests(&fixture.log, Format::Chat);
    let previous_request_count = requests.len();
    let before = requests.last().expect("skill-enabled request").clone();
    assert!(system_text(&before, Format::Chat).contains("PREFIX_SKILL_STATE_DESCRIPTION"));
    shutdown(agent).await;

    let (state_path, state) =
        pig_utils::skills::skills_state_after_toggle(&fixture.data_dir, &skill_md, false).unwrap();
    pig_core::config::write_settings_file(&state_path, state.as_bytes()).unwrap();
    let agent = fixture.spawn();
    open_replayed(&agent, &session_id).await;
    send_and_complete(&agent, &agent.events, &session_id, "skill disabled").await;
    let requests = main_requests(&fixture.log, Format::Chat);
    assert!(requests.len() > previous_request_count);
    let after = requests.last().expect("skill-disabled request");
    assert!(!system_text(after, Format::Chat).contains("PREFIX_SKILL_STATE_DESCRIPTION"));
    shutdown(agent).await;
}
