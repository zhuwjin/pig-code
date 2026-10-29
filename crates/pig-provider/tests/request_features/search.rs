use super::*;
use pig_provider::{AssistantReplayItem, mock, replay_sources};

// ---------------- Native hosted search ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_native_search_replays_and_derives_sources() {
    let (port, log) = mock::start_mock_server_with_log();
    let mut config = model(port, ApiFormat::OpenAiResponses);
    config.cap_web_search = true;
    config.replay_native_search = true;
    config.prompt_cache_key = Some("pig:session-1".into());

    let first_user = ChatMsg::user(format!(
        "{} tell me about pig code",
        mock::RESPONSES_WEB_SEARCH_TRIGGER
    ));
    let (first_body, first_events) = run(
        config.clone(),
        vec![first_user.clone()],
        vec![read_tool()],
        &log,
    )
    .await;
    let first_body = first_body.expect("first body");
    assert_failure_free(&first_events);
    assert!(
        first_body
            .get("include")
            .is_none_or(|include| !include.as_array().is_some_and(|items| items
                .iter()
                .any(|value| value == "web_search_call.action.sources"))),
        "compatible Responses endpoints do not get the OpenAI-only include: {first_body}"
    );
    assert_eq!(first_body["store"], false);

    let replay = replay_of(&first_events);
    assert!(
        replay.items.iter().any(|item| matches!(
            item,
            AssistantReplayItem::ResponsesWebSearchCall {
                id: Some(id),
                action,
                encrypted_content: None,
                ..
            } if id == "ws_1" && action["query"] == "pig code"
        )),
        "the native call keeps the real id/status/action shape: {replay:?}"
    );
    let sources = replay_sources(&replay);
    assert_eq!(sources.len(), 2, "{sources:?}");
    assert_eq!(sources[0].url, "https://example.com/action");
    assert_eq!(sources[1].url, "https://example.com/cite");
    let serialized = serde_json::to_string(&sources).unwrap();
    assert!(!serialized.contains("encrypted"), "{serialized}");

    let second_messages = vec![
        first_user,
        ChatMsg::assistant_with_replay(String::new(), vec![], None, Some(replay.clone())),
        ChatMsg::user("continue".into()),
    ];
    let (second_body, _) = run(
        config.clone(),
        second_messages.clone(),
        vec![read_tool()],
        &log,
    )
    .await;
    let second_body = second_body.expect("second body");
    let input = second_body["input"].as_array().unwrap();
    let search = input
        .iter()
        .find(|item| item["type"] == "web_search_call")
        .expect("native search call replayed");
    assert_eq!(search["id"], "ws_1");
    assert_eq!(search["status"], "completed");
    assert_eq!(search["action"]["query"], "pig code");
    assert!(
        search.get("encrypted_content").is_none(),
        "no invented credential: {search}"
    );
    let assistant = input
        .iter()
        .find(|item| item["type"] == "message" && item["role"] == "assistant")
        .expect("assistant message replayed");
    let annotation = &assistant["content"][0]["annotations"][0];
    assert_eq!(annotation["type"], "url_citation");
    assert_eq!(annotation["url"], "https://example.com/cite");
    assert_eq!(second_body["store"], false);

    // Compatible endpoints replay text only, but the stored replay still
    // yields the sources above.
    let mut compatible = config;
    compatible.replay_native_search = false;
    let (text_only, _) = run(compatible, second_messages, vec![read_tool()], &log).await;
    let text_only = text_only.expect("compatible body");
    let input = text_only["input"].to_string();
    assert!(!input.contains("web_search_call"), "{text_only}");
    assert!(!input.contains("annotations"), "{text_only}");
    assert!(
        input.contains(mock::RESPONSES_WEB_SEARCH_TEXT),
        "{text_only}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_native_search_replays_and_derives_sources() {
    let (port, log) = mock::start_mock_server_with_log();
    let mut config = model(port, ApiFormat::AnthropicMessages);
    config.cap_web_search = true;
    config.replay_native_search = true;

    let first_user = ChatMsg::user(format!(
        "{} tell me about pig code",
        mock::ANTHROPIC_WEB_SEARCH_TRIGGER
    ));
    let (_, first_events) = run(
        config.clone(),
        vec![first_user.clone()],
        vec![read_tool()],
        &log,
    )
    .await;
    assert_failure_free(&first_events);
    let replay = replay_of(&first_events);
    assert!(
        replay.items.iter().any(|item| matches!(
            item,
            AssistantReplayItem::AnthropicServerToolUse { input, .. }
                if input["query"] == "pig code"
        )),
        "server tool input is collected separately: {replay:?}"
    );
    assert!(
        replay.items.iter().any(|item| matches!(
            item,
            AssistantReplayItem::AnthropicWebSearchResult { content, .. }
                if content[0]["encrypted_content"] == "encrypted-result"
        )),
        "the native result keeps its credential: {replay:?}"
    );
    let sources = replay_sources(&replay);
    assert_eq!(sources.len(), 2, "{sources:?}");
    assert_eq!(sources[0].url, "https://example.com/action");
    assert_eq!(sources[1].url, "https://example.com/cite");
    assert!(
        !serde_json::to_string(&sources)
            .unwrap()
            .contains("encrypted")
    );

    let second_messages = vec![
        first_user,
        ChatMsg::assistant_with_replay(String::new(), vec![], None, Some(replay)),
        ChatMsg::user("continue".into()),
    ];
    let (second_body, _) = run(
        config.clone(),
        second_messages.clone(),
        vec![read_tool()],
        &log,
    )
    .await;
    let second_body = second_body.expect("second body");
    let assistant = second_body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("assistant message");
    let blocks = assistant["content"].as_array().unwrap();
    let server = blocks
        .iter()
        .find(|block| block["type"] == "server_tool_use")
        .expect("server_tool_use replayed");
    assert_eq!(server["id"], "srvtoolu_1");
    assert_eq!(server["input"]["query"], "pig code");
    let result = blocks
        .iter()
        .find(|block| block["type"] == "web_search_tool_result")
        .expect("web_search_tool_result replayed");
    assert_eq!(
        result["content"][0]["encrypted_content"],
        "encrypted-result"
    );
    let text = blocks
        .iter()
        .find(|block| block["type"] == "text")
        .expect("text replayed");
    assert_eq!(text["citations"][0]["encrypted_index"], "encrypted-index");

    let mut compatible = config;
    compatible.replay_native_search = false;
    let (text_only, _) = run(compatible, second_messages, vec![read_tool()], &log).await;
    let text_only = text_only.expect("compatible body");
    let serialized = text_only.to_string();
    assert!(!serialized.contains("server_tool_use"), "{text_only}");
    assert!(
        !serialized.contains("web_search_tool_result"),
        "{text_only}"
    );
    assert!(!serialized.contains("\"citations\""), "{text_only}");
    assert!(
        serialized.contains(mock::ANTHROPIC_WEB_SEARCH_TEXT),
        "{text_only}"
    );
}
