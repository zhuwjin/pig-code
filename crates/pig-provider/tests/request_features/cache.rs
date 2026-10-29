use super::*;
use pig_provider::{complete_messages, complete_text, mock};

// ---------------- Cache keys and breakpoints ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_cache_key_rides_chat_and_responses_but_not_sidecars() {
    let (port, log) = mock::start_mock_server_with_log();
    for format in [ApiFormat::OpenAiChat, ApiFormat::OpenAiResponses] {
        let mut config = model(port, format);
        config.prompt_cache_key = Some("pig:session-1".into());

        let (first, _) = run(
            config.clone(),
            vec![ChatMsg::user("hello".into())],
            vec![],
            &log,
        )
        .await;
        let first = first.expect("first body");
        assert_eq!(first["prompt_cache_key"], "pig:session-1", "{format:?}");

        let (second, _) = run(
            config.clone(),
            vec![ChatMsg::user("hello".into())],
            vec![],
            &log,
        )
        .await;
        assert_eq!(
            second.expect("second body")["prompt_cache_key"],
            "pig:session-1",
            "the routing key is stable across retries: {format:?}"
        );

        let before = log.lock().expect("log").len();
        complete_messages(
            &config,
            &[ChatMsg::user("summarize".into())],
            &[],
            &CancellationToken::new(),
        )
        .await
        .expect("compact sidecar succeeds");
        let sidecar = last_sidecar_body(&log, before);
        assert_eq!(
            sidecar["prompt_cache_key"], "pig:session-1",
            "the compact sidecar shares the main session key: {format:?}"
        );

        let before = log.lock().expect("log").len();
        complete_text(&config, "title".into(), None, &CancellationToken::new())
            .await
            .expect("title sidecar succeeds");
        let title = last_sidecar_body(&log, before);
        assert!(
            title.get("prompt_cache_key").is_none(),
            "title/connectivity one-shots never carry the key: {format:?}/{title}"
        );
    }

    let (port, log) = mock::start_mock_server_with_log();
    let mut config = model(port, ApiFormat::AnthropicMessages);
    config.prompt_cache_key = Some("pig:session-1".into());
    let (body, _) = run(config, vec![ChatMsg::user("hello".into())], vec![], &log).await;
    assert!(
        body.expect("body").get("prompt_cache_key").is_none(),
        "Anthropic has no prompt_cache_key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_live_and_compact_share_the_exact_prefix() {
    let (port, log) = mock::start_mock_server_with_log();
    let mut config = model(port, ApiFormat::AnthropicMessages);
    config.anthropic_cache_control = true;
    config.reasoning_params = Some(json!({
        "thinking": {"type": "enabled"},
        "output_config": {"effort": "high"},
    }));
    let history = populated_history();
    let tools = vec![read_tool()];

    let (live, live_events) = run(config.clone(), history.clone(), tools.clone(), &log).await;
    let live = live.expect("live body");
    assert_failure_free(&live_events);
    assert_eq!(
        cache_breakpoints(&live),
        3,
        "tools + system + the last message each carry one breakpoint: {live}"
    );
    assert_eq!(
        live["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(
        live["system"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert!(
        live["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .to_string()
            .contains("cache_control"),
        "the live tail is marked: {live}"
    );
    assert!(!live.to_string().contains("ttl"), "no TTL: {live}");

    let mut sidecar_messages = history;
    sidecar_messages.push(ChatMsg::user("summarize the conversation".into()));
    let before = log.lock().expect("log").len();
    complete_messages(
        &config,
        &sidecar_messages,
        &tools,
        &CancellationToken::new(),
    )
    .await
    .expect("compact sidecar succeeds");
    let sidecar = last_sidecar_body(&log, before);
    assert_eq!(cache_breakpoints(&sidecar), 3, "{sidecar}");
    assert_eq!(
        sidecar["system"], live["system"],
        "system bytes (including the breakpoint) must match the live prefix"
    );
    assert_eq!(
        sidecar["tools"], live["tools"],
        "tools bytes must match the live prefix"
    );
    assert_eq!(
        sidecar["thinking"], live["thinking"],
        "reasoning config belongs to the cached prefix"
    );
    let live_messages = live["messages"].as_array().unwrap();
    let sidecar_messages = sidecar["messages"].as_array().unwrap();
    assert_eq!(sidecar_messages.len(), live_messages.len() + 1);
    assert_eq!(
        &sidecar_messages[..live_messages.len()],
        &live_messages[..],
        "the original history including the tail breakpoint is byte-identical"
    );
    let instruction = sidecar_messages.last().unwrap();
    assert_eq!(instruction["role"], "user");
    assert!(
        !instruction.to_string().contains("cache_control"),
        "the appended compact instruction must not be cached: {instruction}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_cache_can_be_disabled_and_never_marks_hosted_tools() {
    let (port, log) = mock::start_mock_server_with_log();
    let mut config = model(port, ApiFormat::AnthropicMessages);
    let (plain, _) = run(config.clone(), populated_history(), vec![read_tool()], &log).await;
    let plain = plain.expect("body");
    assert_eq!(cache_breakpoints(&plain), 0, "{plain}");
    assert!(plain["system"].is_string(), "legacy system shape: {plain}");

    config.anthropic_cache_control = true;
    config.cap_web_search = true;
    let (with_search, _) = run(config, populated_history(), vec![read_tool()], &log).await;
    let with_search = with_search.expect("body");
    let tools = with_search["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "Read");
    assert_eq!(tools[0]["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(tools[1]["type"], "web_search_20250305");
    assert!(
        tools[1].get("cache_control").is_none(),
        "the hosted tool definition is never a cache anchor: {with_search}"
    );
}
