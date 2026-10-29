//! Anthropic `pause_turn` is a typed continuation boundary, not a normal
//! stop. These tests run against the real in-process HTTP mock and inspect the
//! persisted replay plus the very next request.

use pig_protocol::{ApiFormat, FinishReason};
use pig_provider::mock;
use pig_provider::{
    AssistantReplay, AssistantReplayItem, CallControl, ChatMsg, ProviderEvent, ResolvedModel,
    ToolCallSettings, replay_sources, stream_chat,
};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

type RequestLog = Arc<Mutex<Vec<String>>>;

fn model(port: u16) -> ResolvedModel {
    ResolvedModel {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "mock-key".into(),
        model: "claude-sonnet-4-5".into(),
        context_window: 128_000,
        max_output_tokens: 8_192,
        api_format: ApiFormat::AnthropicMessages,
        reasoning_params: None,
        cap_structured: false,
        cap_strict_tools: false,
        cap_web_search: true,
        web_search_tool: None,
        input_image: false,
        input_pdf: false,
        anthropic_cache_control: false,
        replay_native_search: false,
        prompt_cache_key: None,
        provider_name: "mock".into(),
        tool_settings: ToolCallSettings::new(true),
    }
}

async fn run(
    config: ResolvedModel,
    messages: Vec<ChatMsg>,
    log: &RequestLog,
) -> (Option<Value>, Vec<ProviderEvent>) {
    let before = log.lock().expect("log").len();
    let format = config.api_format;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(stream_chat(
        config,
        messages,
        Vec::new(),
        tx,
        CallControl::new(CancellationToken::new()),
    ));
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    task.await.expect("stream task joins");
    let body = log
        .lock()
        .expect("log")
        .get(before)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    if format == ApiFormat::AnthropicMessages
        && let Some(body) = &body
    {
        mock::validate_anthropic_request(body)
            .unwrap_or_else(|problem| panic!("invalid Anthropic request shape: {problem}\n{body}"));
    }
    (body, events)
}

fn replay_of(events: &[ProviderEvent]) -> AssistantReplay {
    events
        .iter()
        .find_map(|event| match event {
            ProviderEvent::AssistantReplay(replay) => Some(replay.clone()),
            _ => None,
        })
        .expect("assistant replay")
}

fn finished(events: &[ProviderEvent]) -> FinishReason {
    events
        .iter()
        .find_map(|event| match event {
            ProviderEvent::Finished(reason) => Some(*reason),
            _ => None,
        })
        .expect("finished event")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_turn_is_typed_and_keeps_native_replay() {
    let (port, log) = mock::start_mock_server_with_log();
    let messages = vec![
        ChatMsg::system("system".into()),
        ChatMsg::user(format!(
            "{} search then pause",
            mock::ANTHROPIC_PAUSE_ONCE_TRIGGER
        )),
    ];
    let (body, events) = run(model(port), messages.clone(), &log).await;
    assert!(body.is_some(), "pause request reached the mock");
    assert_eq!(
        finished(&events),
        FinishReason::Pause,
        "pause_turn must not be Stop"
    );
    assert_ne!(finished(&events), FinishReason::Length);
    assert!(!finished(&events).is_incomplete());

    let replay = replay_of(&events);
    assert!(
        replay.items.iter().any(|item| matches!(
            item,
            AssistantReplayItem::AnthropicThinking { thinking, signature: Some(signature) }
                if thinking == "pause reasoning 1" && signature == "sig-pause-1"
        )),
        "thinking replay retained: {replay:?}"
    );
    assert!(
        replay.items.iter().any(|item| matches!(
            item,
            AssistantReplayItem::AnthropicServerToolUse { id, name, input }
                if id == "srvtoolu_pause_1" && name == "web_search" && input["query"] == "pause"
        )),
        "server tool use retained: {replay:?}"
    );
    assert!(
        replay.items.iter().any(|item| matches!(
            item,
            AssistantReplayItem::AnthropicWebSearchResult { tool_use_id, content }
                if tool_use_id == "srvtoolu_pause_1"
                    && content[0]["url"] == mock::ANTHROPIC_PAUSE_SOURCE_URL
                    && content[0]["encrypted_content"] == "encrypted-pause-1"
        )),
        "web search result retained: {replay:?}"
    );
    assert!(
        replay.items.iter().any(|item| matches!(
            item,
            AssistantReplayItem::Text { text, .. } if text == mock::ANTHROPIC_PAUSE_ONCE_CONTENT
        )),
        "paused answer text retained: {replay:?}"
    );
    let sources = replay_sources(&replay);
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].url, mock::ANTHROPIC_PAUSE_SOURCE_URL);
    assert_eq!(sources[0].title, "Pause source");

    // The next request in the same turn carries the paused assistant verbatim;
    // with native replay enabled the server blocks and their credentials ride
    // along in their original order.
    let mut native = model(port);
    native.replay_native_search = true;
    let continuation = vec![
        ChatMsg::system("system".into()),
        ChatMsg::user(format!(
            "{} search then pause",
            mock::ANTHROPIC_PAUSE_ONCE_TRIGGER
        )),
        ChatMsg::assistant_with_replay(
            mock::ANTHROPIC_PAUSE_ONCE_CONTENT.into(),
            vec![],
            None,
            Some(replay),
        ),
    ];
    let (second_body, second_events) = run(native, continuation, &log).await;
    assert_eq!(finished(&second_events), FinishReason::Stop);
    let second_body = second_body.expect("continuation request");
    let input = second_body["messages"].to_string();
    assert!(input.contains("server_tool_use"), "{second_body}");
    assert!(input.contains("web_search_tool_result"), "{second_body}");
    assert!(input.contains("sig-pause-1"), "{second_body}");
    assert!(input.contains("encrypted-pause-1"), "{second_body}");
    assert_eq!(
        input.matches(mock::ANTHROPIC_PAUSE_ONCE_CONTENT).count(),
        1,
        "the paused assistant is not duplicated: {second_body}"
    );
}

/// Existing Anthropic terminal reasons keep their old meanings. The `end_turn`
/// half is covered by the pause continuation above (its second response ends
/// with `end_turn` and asserts Stop).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_sequence_stays_stop() {
    let (port, log) = mock::start_mock_server_with_log();
    let (_, stop_sequence) = run(
        model(port),
        vec![
            ChatMsg::system("system".into()),
            ChatMsg::user(mock::ANTHROPIC_STOP_SEQUENCE_TRIGGER.into()),
        ],
        &log,
    )
    .await;
    assert_eq!(finished(&stop_sequence), FinishReason::Stop);
}
