use super::*;

#[derive(Deserialize)]
pub(crate) struct CompleteResponse {
    choices: Option<Vec<CompleteChoice>>,
    content: Option<Vec<CompleteBlock>>,
}

#[derive(Deserialize)]
pub(crate) struct CompleteChoice {
    message: Option<CompleteMessage>,
}

#[derive(Deserialize)]
pub(crate) struct CompleteMessage {
    content: Option<String>,
    /// Some = the model tried to call a tool while summarizing (forbidden by the summary instructions); the caller should fall back to the truncation path
    tool_calls: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(crate) struct CompleteBlock {
    text: Option<String>,
    /// Block type: tool_use = the model tried to call a tool while summarizing (see above)
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// Non-streaming one-shot request (compaction summary).
pub async fn complete_text(
    config: &ResolvedModel,
    user_content: String,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String, CoreError> {
    let client = http_client();
    match config.api_format {
        ApiFormat::OpenAiChat => {
            let body = serde_json::json!({
                "model": config.model,
                "messages": [{"role": "user", "content": user_content}],
                "stream": false,
                "max_tokens": config.max_output_tokens,
            });
            let url = format!("{}/chat/completions", config.base_url);
            let mut api =
                crate::api_log::ApiCall::new("openai.chat", &config.provider_name, &config.model);
            api.request(&url, &body);
            let response = tokio::select! {
                result = client
                    .post(&url)
                    .bearer_auth(&config.api_key)
                    .json(&body)
                    .send() => result.map_err(net_err),
                _ = cancel.cancelled() => Err(CoreError::Internal { detail: "Cancelled".to_string() }),
            };
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    api.fail(&core_error_en(&error));
                    return Err(error);
                }
            };
            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                let detail: String = detail.chars().take(300).collect();
                api.fail(&format!("HTTP {status}: {detail}"));
                return Err(CoreError::Internal {
                    detail: format!("HTTP {status}: {detail}"),
                });
            }
            let parsed: CompleteResponse = {
                let raw = response.text().await.map_err(|e| CoreError::Internal {
                    detail: format!("Failed to read response: {e}"),
                })?;
                api.raw_line(&raw);
                serde_json::from_str(&raw).map_err(|e| CoreError::Internal {
                    detail: format!("Failed to parse response: {e}"),
                })?
            };
            let result = parsed
                .choices
                .and_then(|mut c| c.pop())
                .and_then(|c| c.message)
                .and_then(|m| m.content)
                .filter(|content| !content.is_empty())
                .ok_or_else(|| CoreError::Internal {
                    detail: "Response has no content".to_string(),
                });
            match &result {
                Ok(text) => {
                    let mut api = api;
                    api.text(text);
                    api.finish("stop", &[]);
                }
                Err(error) => api.fail(&core_error_en(error)),
            }
            result
        }
        ApiFormat::AnthropicMessages => {
            let body = serde_json::json!({
                "model": config.model,
                "max_tokens": config.max_output_tokens,
                "stream": false,
                "messages": [{"role": "user", "content": user_content}],
            });
            let url = anthropic_url(&config.base_url);
            let mut api = crate::api_log::ApiCall::new(
                "anthropic.messages",
                &config.provider_name,
                &config.model,
            );
            api.request(&url, &body);
            let response = tokio::select! {
                result = client
                    .post(&url)
                    .header("x-api-key", &config.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send() => result.map_err(net_err),
                _ = cancel.cancelled() => Err(CoreError::Internal { detail: "Cancelled".to_string() }),
            };
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    api.fail(&core_error_en(&error));
                    return Err(error);
                }
            };
            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                let detail: String = detail.chars().take(300).collect();
                api.fail(&format!("HTTP {status}: {detail}"));
                return Err(CoreError::Internal {
                    detail: format!("HTTP {status}: {detail}"),
                });
            }
            let parsed: CompleteResponse = {
                let raw = response.text().await.map_err(|e| CoreError::Internal {
                    detail: format!("Failed to read response: {e}"),
                })?;
                api.raw_line(&raw);
                serde_json::from_str(&raw).map_err(|e| CoreError::Internal {
                    detail: format!("Failed to parse response: {e}"),
                })?
            };
            let result = parsed
                .content
                .and_then(|blocks| blocks.into_iter().find_map(|b| b.text))
                .filter(|text| !text.is_empty())
                .ok_or_else(|| CoreError::Internal {
                    detail: "Response has no content".to_string(),
                });
            match &result {
                Ok(text) => {
                    let mut api = api;
                    api.text(text);
                    api.finish("stop", &[]);
                }
                Err(error) => api.fail(&core_error_en(error)),
            }
            result
        }
    }
}

/// Non-streaming one-shot request (compaction summary), multi-message form: full message list + tool list.
/// The same system/tools/history bytes as the session request -> OpenAI-family automatic prefix caching hits the cache written
/// by the previous turn (same trade-off as ZCode's same-pipeline projection / kimi-code's same history array).
/// If the model returns tool calls while summarizing (forbidden by the instructions), treat it as a failure; the caller falls back to the truncation path.
pub async fn complete_messages(
    config: &ResolvedModel,
    messages: &[ChatMsg],
    tools: &[serde_json::Value],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String, CoreError> {
    let client = http_client();
    match config.api_format {
        ApiFormat::OpenAiChat => {
            // The same tool assembly as the streaming path (server-side search tool injected per capability), so the cache prefix aligns
            let tools = openai_request_tools(config, tools);
            let mut body = serde_json::json!({
                "model": config.model,
                "messages": to_openai_messages(messages),
                "stream": false,
                "max_tokens": config.max_output_tokens,
            });
            if !tools.is_empty() {
                body["tools"] = serde_json::Value::Array(tools);
            }
            let url = format!("{}/chat/completions", config.base_url);
            let mut api =
                crate::api_log::ApiCall::new("openai.chat", &config.provider_name, &config.model);
            api.request(&url, &body);
            let response = tokio::select! {
                result = client
                    .post(&url)
                    .bearer_auth(&config.api_key)
                    .json(&body)
                    .send() => result.map_err(net_err),
                _ = cancel.cancelled() => Err(CoreError::Internal { detail: "Cancelled".to_string() }),
            };
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    api.fail(&core_error_en(&error));
                    return Err(error);
                }
            };
            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                let detail: String = detail.chars().take(300).collect();
                api.fail(&format!("HTTP {status}: {detail}"));
                return Err(CoreError::Internal {
                    detail: format!("HTTP {status}: {detail}"),
                });
            }
            let parsed: CompleteResponse = {
                let raw = response.text().await.map_err(|e| CoreError::Internal {
                    detail: format!("Failed to read response: {e}"),
                })?;
                api.raw_line(&raw);
                serde_json::from_str(&raw).map_err(|e| CoreError::Internal {
                    detail: format!("Failed to parse response: {e}"),
                })?
            };
            let result = (|| {
                let message = parsed
                    .choices
                    .and_then(|mut c| c.pop())
                    .and_then(|c| c.message)
                    .ok_or_else(|| CoreError::Internal {
                        detail: "Response has no message".to_string(),
                    })?;
                if message.tool_calls.is_some() {
                    return Err(CoreError::Internal {
                        detail: "Summary response contains tool calls".to_string(),
                    });
                }
                message
                    .content
                    .filter(|content| !content.is_empty())
                    .ok_or_else(|| CoreError::Internal {
                        detail: "Response has no content".to_string(),
                    })
            })();
            match &result {
                Ok(text) => {
                    let mut api = api;
                    api.text(text);
                    api.finish("stop", &[]);
                }
                Err(error) => api.fail(&core_error_en(error)),
            }
            result
        }
        ApiFormat::AnthropicMessages => {
            let (system, msgs) = to_anthropic_messages(messages);
            // The same tool assembly as the streaming path (OpenAI wire shape -> Anthropic shape +
            // the server-side search tool when the capability is on). Passing root_schemas' OpenAI
            // wire shape through directly gets rejected with 422 by Anthropic-compatible endpoints (Kimi errors in practice)
            let anthropic_tools = anthropic_request_tools(config, tools);
            let mut body = serde_json::json!({
                "model": config.model,
                "max_tokens": config.max_output_tokens,
                "stream": false,
                "messages": msgs,
            });
            if !system.is_empty() {
                body["system"] = serde_json::Value::String(system);
            }
            if !anthropic_tools.is_empty() {
                body["tools"] = serde_json::Value::Array(anthropic_tools);
            }
            let url = anthropic_url(&config.base_url);
            let mut api = crate::api_log::ApiCall::new(
                "anthropic.messages",
                &config.provider_name,
                &config.model,
            );
            api.request(&url, &body);
            let response = tokio::select! {
                result = client
                    .post(&url)
                    .header("x-api-key", &config.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send() => result.map_err(net_err),
                _ = cancel.cancelled() => Err(CoreError::Internal { detail: "Cancelled".to_string() }),
            };
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    api.fail(&core_error_en(&error));
                    return Err(error);
                }
            };
            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                let detail: String = detail.chars().take(300).collect();
                api.fail(&format!("HTTP {status}: {detail}"));
                return Err(CoreError::Internal {
                    detail: format!("HTTP {status}: {detail}"),
                });
            }
            let parsed: CompleteResponse = {
                let raw = response.text().await.map_err(|e| CoreError::Internal {
                    detail: format!("Failed to read response: {e}"),
                })?;
                api.raw_line(&raw);
                serde_json::from_str(&raw).map_err(|e| CoreError::Internal {
                    detail: format!("Failed to parse response: {e}"),
                })?
            };
            let result = (|| {
                let blocks = parsed.content.ok_or_else(|| CoreError::Internal {
                    detail: "Response has no content".to_string(),
                })?;
                if blocks.iter().any(|b| b.kind.as_deref() == Some("tool_use")) {
                    return Err(CoreError::Internal {
                        detail: "Summary response contains tool calls".to_string(),
                    });
                }
                blocks
                    .into_iter()
                    .find_map(|b| b.text)
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| CoreError::Internal {
                        detail: "Response has no content".to_string(),
                    })
            })();
            match &result {
                Ok(text) => {
                    let mut api = api;
                    api.text(text);
                    api.finish("stop", &[]);
                }
                Err(error) => api.fail(&core_error_en(error)),
            }
            result
        }
    }
}

/// Connectivity test: minimal request, any 2xx passes. The result is carried as a structured ConnTestResult (localized in the UI).
pub async fn test_provider(
    base_url: &str,
    api_key: &str,
    format: ApiFormat,
    model: &str,
) -> ConnTestResult {
    let client = http_client();
    let api = crate::api_log::ApiCall::new("conn.test", "", model);
    let send = async {
        match format {
            ApiFormat::OpenAiChat => {
                let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
                let body = serde_json::json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "ping"}],
                    "max_tokens": 1,
                    "stream": false,
                });
                api.request(&url, &body);
                client
                    .post(&url)
                    .bearer_auth(api_key)
                    .json(&body)
                    .send()
                    .await
            }
            ApiFormat::AnthropicMessages => {
                let url = anthropic_url(base_url.trim_end_matches('/'));
                let body = serde_json::json!({
                    "model": model,
                    "max_tokens": 1,
                    "messages": [{"role": "user", "content": "ping"}],
                    "stream": false,
                });
                api.request(&url, &body);
                client
                    .post(&url)
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
                    .await
            }
        }
    };
    const TIMEOUT_SECS: u64 = 10;
    let response =
        match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_SECS), send).await {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                // Network errors fold in the root cause and become the failure detail (same policy as streaming requests)
                let CoreError::Network { detail } = net_err(e) else {
                    unreachable!("net_err is always Network")
                };
                api.fail(&format!("network: {detail}"));
                return ConnTestResult::Failed { detail };
            }
            Err(_) => {
                api.fail(&format!("timeout after {TIMEOUT_SECS}s"));
                return ConnTestResult::Timeout { secs: TIMEOUT_SECS };
            }
        };
    let status = response.status();
    if status.is_success() {
        api.finish(&format!("http {status}"), &[]);
        ConnTestResult::Connected {
            status: status.as_u16(),
        }
    } else {
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(200).collect();
        api.fail(&format!("HTTP {status}: {detail}"));
        ConnTestResult::Failed {
            detail: format!("HTTP {status}: {detail}"),
        }
    }
}

/// Command-line network probe (triggered by PIG_NET_TEST=1 in pig-app, opens no window):
/// reads the app's real config and probes each enabled provider's models for connectivity one by one (ping + a real streaming request),
/// printing the results, for network troubleshooting.
pub fn net_test_blocking(config_path: &std::path::Path) {
    let config = match crate::config::load(config_path) {
        Ok(config) => config,
        Err(e) => {
            println!(
                "[net-test] Failed to read config ({}): {e:?}",
                config_path.display()
            );
            return;
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut tested = 0;
    for provider in config.providers.iter().filter(|p| p.enabled) {
        for model in &provider.models {
            tested += 1;
            let api_key = crate::config::expand_env(&provider.api_key);
            let result = rt.block_on(test_provider(
                &provider.base_url,
                &api_key,
                provider.api_format,
                &model.id,
            ));
            println!(
                "[net-test] ping {}/{} ({}) → {result:?}",
                provider.name, model.id, provider.base_url
            );

            // The exact same streaming path as sending a message
            let resolved = ResolvedModel {
                base_url: provider.base_url.clone(),
                api_key,
                model: model.id.clone(),
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
                api_format: provider.api_format,
                reasoning_params: None,
                cap_web_search: model.cap_web_search,
                web_search_tool: model.web_search_tool.clone(),
                input_image: model.input_image,
                provider_name: provider.name.clone(),
            };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let cancel = tokio_util::sync::CancellationToken::new();
            rt.block_on(stream_chat(
                resolved,
                vec![ChatMsg::user("ping".to_string())],
                Vec::new(),
                tx,
                cancel,
            ));
            let mut outcome = "Finished".to_string();
            while let Ok(event) = rx.try_recv() {
                match event {
                    ProviderEvent::Failed(e) => outcome = format!("Failed: {e:?}"),
                    ProviderEvent::ToolCalls(_) => outcome = "ToolCalls".to_string(),
                    _ => {}
                }
            }
            println!(
                "[net-test] stream {}/{} → {outcome}",
                provider.name, model.id
            );
        }
    }
    if tested == 0 {
        println!("[net-test] no enabled provider model");
    }
}
