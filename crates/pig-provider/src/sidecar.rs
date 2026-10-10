use serde::Deserialize;

use pig_protocol::{ApiFormat, ConnTestResult, CoreError};

use crate::anthropic::{anthropic_request_tools, anthropic_url, to_anthropic_messages};
use crate::chat::{ChatMsg, ResolvedModel};
use crate::core_error_en;
use crate::identity::apply_sdk_headers;
use crate::openai::{openai_request_tools, to_openai_messages};
use crate::retry::{http_client, net_err};

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
                result = apply_sdk_headers(
                    client.post(&url).bearer_auth(&config.api_key),
                    false,
                    false,
                    0,
                )
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
                result = apply_sdk_headers(
                    client
                        .post(&url)
                        .header("x-api-key", &config.api_key)
                        .header("anthropic-version", "2023-06-01"),
                    true,
                    false,
                    0,
                )
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
        ApiFormat::OpenAiResponses => {
            let body = serde_json::json!({
                "model": config.model,
                "input": [{"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": user_content}
                ]}],
                "store": false,
                "stream": false,
                "max_output_tokens": config.max_output_tokens,
            });
            let url = responses_url(&config.base_url);
            let mut api = crate::api_log::ApiCall::new(
                "openai.responses",
                &config.provider_name,
                &config.model,
            );
            api.request(&url, &body);
            let response = tokio::select! {
                result = apply_sdk_headers(
                    client.post(&url).bearer_auth(&config.api_key),
                    false,
                    false,
                    0,
                )
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
            let raw = response.text().await.map_err(|e| CoreError::Internal {
                detail: format!("Failed to read response: {e}"),
            })?;
            api.raw_line(&raw);
            let result = responses_output_text(&raw);
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
                result = apply_sdk_headers(
                    client.post(&url).bearer_auth(&config.api_key),
                    false,
                    false,
                    0,
                )
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
                result = apply_sdk_headers(
                    client
                        .post(&url)
                        .header("x-api-key", &config.api_key)
                        .header("anthropic-version", "2023-06-01"),
                    true,
                    false,
                    0,
                )
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
        ApiFormat::OpenAiResponses => {
            // The same input/tool assembly as the streaming path, so the
            // cache prefix aligns with the previous turn's request
            let (instructions, input) = crate::responses::to_responses_input(messages);
            let tools = crate::responses::responses_request_tools(config, tools);
            let mut body = serde_json::json!({
                "model": config.model,
                "input": input,
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "store": false,
                "stream": false,
                "max_output_tokens": config.max_output_tokens,
            });
            if !instructions.is_empty() {
                body["instructions"] = serde_json::Value::String(instructions);
            }
            if !tools.is_empty() {
                body["tools"] = serde_json::Value::Array(tools);
            }
            crate::chat::merge_reasoning_params(&mut body, config);
            let url = responses_url(&config.base_url);
            let mut api = crate::api_log::ApiCall::new(
                "openai.responses",
                &config.provider_name,
                &config.model,
            );
            api.request(&url, &body);
            let response = tokio::select! {
                result = apply_sdk_headers(
                    client.post(&url).bearer_auth(&config.api_key),
                    false,
                    false,
                    0,
                )
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
            let raw = response.text().await.map_err(|e| CoreError::Internal {
                detail: format!("Failed to read response: {e}"),
            })?;
            api.raw_line(&raw);
            // A function_call item in the summary output is forbidden (same
            // contract as the other formats)
            let result = (|| {
                let value: serde_json::Value =
                    serde_json::from_str(&raw).map_err(|e| CoreError::Internal {
                        detail: format!("Failed to parse response: {e}"),
                    })?;
                if value["output"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["type"] == "function_call"))
                {
                    return Err(CoreError::Internal {
                        detail: "Summary response contains tool calls".to_string(),
                    });
                }
                responses_output_text(&raw)
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
    let mut api = crate::api_log::ApiCall::new("conn.test", "", model);
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
                apply_sdk_headers(client.post(&url).bearer_auth(api_key), false, false, 0)
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
                apply_sdk_headers(
                    client
                        .post(&url)
                        .header("x-api-key", api_key)
                        .header("anthropic-version", "2023-06-01"),
                    true,
                    false,
                    0,
                )
                .json(&body)
                .send()
                .await
            }
            ApiFormat::OpenAiResponses => {
                let url = responses_url(base_url.trim_end_matches('/'));
                let body = serde_json::json!({
                    "model": model,
                    "input": [{"type": "message", "role": "user", "content": [
                        {"type": "input_text", "text": "ping"}
                    ]}],
                    "store": false,
                    "stream": false,
                    "max_output_tokens": 16,
                });
                api.request(&url, &body);
                apply_sdk_headers(client.post(&url).bearer_auth(api_key), false, false, 0)
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

/// Responses endpoint URL: `{base}/responses` (no /v1 games — the Responses
/// base is the same root Chat Completions uses)
fn responses_url(base: &str) -> String {
    format!("{}/responses", base.trim_end_matches('/'))
}

/// Non-streaming Responses reply -> the joined output_text of the message
/// items (reasoning/function_call items are skipped)
fn responses_output_text(raw: &str) -> Result<String, CoreError> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| CoreError::Internal {
        detail: format!("Failed to parse response: {e}"),
    })?;
    let text: String = value["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "output_text")
        .filter_map(|part| part["text"].as_str())
        .collect();
    if text.is_empty() {
        return Err(CoreError::Internal {
            detail: "Response has no content".to_string(),
        });
    }
    Ok(text)
}
