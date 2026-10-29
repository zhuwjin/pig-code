use serde::Deserialize;

use pig_protocol::{ApiFormat, ConnTestResult, CoreError};

use crate::chat::{ChatMsg, ResolvedModel};
use crate::core_error_en;
use crate::identity::apply_sdk_headers;
use crate::retry::{http_client, send_error};
use crate::{ProviderResponseMetadata, ProviderUsage};

#[derive(Debug)]
pub struct CompleteCall {
    pub text: String,
    pub usage: Option<ProviderUsage>,
    pub response: ProviderResponseMetadata,
}

#[derive(Debug)]
pub struct CompleteCallError {
    pub error: CoreError,
    pub usage: Option<ProviderUsage>,
    pub response: ProviderResponseMetadata,
}

#[derive(Default)]
struct CompleteCallDetails {
    usage: Option<ProviderUsage>,
    response: ProviderResponseMetadata,
}

fn cancelled_error() -> CoreError {
    CoreError::Internal {
        detail: "Cancelled".to_string(),
    }
}

/// Send one sidecar request through the bounded transient-retry policy. The
/// final response (including an exhausted retryable status) is returned for
/// the caller to classify; cancellation is reported before any body read.
async fn send_sidecar(
    api: &mut crate::api_log::ApiCall,
    build: impl Fn(u32) -> reqwest::RequestBuilder,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<reqwest::Response, CoreError> {
    match crate::retry::send_sidecar_with_retry(build, cancel).await {
        Ok(crate::retry::SendOutcome::Response(response)) => Ok(response),
        Ok(crate::retry::SendOutcome::Cancelled) => {
            api.finish("cancelled", &[]);
            Err(cancelled_error())
        }
        Err(error) => {
            api.fail(&core_error_en(&error));
            Err(error)
        }
    }
}

/// A sidecar body read outcome. The failed variant keeps the partial body so
/// a non-2xx caller can still classify the HTTP error before surfacing it.
enum SidecarBody {
    Text(String),
    Cancelled,
    Failed(crate::transport::BodyReadError),
}

impl SidecarBody {
    /// Successful-response path: a failed read is the call's error.
    fn success(self, api: &mut crate::api_log::ApiCall) -> Result<String, CoreError> {
        match self {
            Self::Text(text) => Ok(text),
            Self::Cancelled => Err(cancelled_error()),
            Self::Failed(failure) => {
                api.fail(&core_error_en(&failure.error));
                Err(failure.error)
            }
        }
    }

    /// Non-2xx path: the HTTP status owns the failure, so a stalled/truncated
    /// error body must not replace a typed classification (for example a
    /// context-window overflow) with a bare stream-read error.
    fn error_detail(self) -> Result<String, CoreError> {
        match self {
            Self::Text(text) => Ok(text),
            Self::Cancelled => Err(cancelled_error()),
            Self::Failed(failure) => {
                tracing::warn!(
                    detail = %core_error_en(&failure.error),
                    "error body read failed; classifying from the partial body"
                );
                Ok(failure.partial)
            }
        }
    }
}

/// Read a sidecar body with the shared idle/cancellation policy. A cancelled
/// read is not an error to log as a failed request: it ends like a cancelled
/// call.
async fn read_sidecar_body(
    api: &mut crate::api_log::ApiCall,
    response: reqwest::Response,
    cancel: &tokio_util::sync::CancellationToken,
    idle: std::time::Duration,
) -> SidecarBody {
    match crate::transport::read_body_with_idle(response, cancel, idle).await {
        Ok(crate::transport::BodyRead::Text(text)) => SidecarBody::Text(text),
        Ok(crate::transport::BodyRead::Cancelled) => {
            api.finish("cancelled", &[]);
            SidecarBody::Cancelled
        }
        Err(failure) => SidecarBody::Failed(failure),
    }
}

fn truncated_detail(detail: &str) -> String {
    detail.chars().take(300).collect()
}

fn responses_usage(usage: &serde_json::Value, context_window: u64) -> Option<ProviderUsage> {
    crate::responses::usage_fields(usage, context_window).map(
        |(input, cache_read, output, used, total, reasoning_output)| ProviderUsage {
            input,
            cache_read,
            output,
            used,
            total,
            reasoning_output,
        },
    )
}

#[derive(Deserialize)]
pub(crate) struct CompleteResponse {
    id: Option<String>,
    choices: Option<Vec<CompleteChoice>>,
    content: Option<Vec<CompleteBlock>>,
    usage: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(crate) struct CompleteChoice {
    message: Option<CompleteMessage>,
}

#[derive(Deserialize)]
pub(crate) struct CompleteMessage {
    /// String, or a text/output_text parts array (z.ai/GLM-style non-streaming
    /// replies)
    content: Option<serde_json::Value>,
    /// Some = the model tried to call a tool while summarizing (forbidden by the summary instructions); the caller should fall back to the truncation path
    tool_calls: Option<serde_json::Value>,
}

/// A genuinely non-empty tool-call payload. `"tool_calls": []` and `null` are
/// empty declarations, not a forbidden summary tool call.
fn message_has_tool_calls(tool_calls: Option<&serde_json::Value>) -> bool {
    match tool_calls {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Array(calls)) => !calls.is_empty(),
        Some(_) => true,
    }
}

/// Join the text of a non-streaming Chat `content` value: a plain string or a
/// parts array carrying `{"type":"text"}` / `{"type":"output_text"}` blocks.
fn content_text(content: Option<&serde_json::Value>) -> Option<String> {
    match content? {
        serde_json::Value::String(text) => (!text.is_empty()).then(|| text.clone()),
        serde_json::Value::Array(parts) => {
            let text: String = parts
                .iter()
                .filter(|part| matches!(part["type"].as_str(), Some("text" | "output_text")))
                .filter_map(|part| part["text"].as_str())
                .collect();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

#[derive(Deserialize)]
pub(crate) struct CompleteBlock {
    text: Option<String>,
    /// Block type: tool_use = the model tried to call a tool while summarizing (see above)
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// One structured-output request: the schema's name and its JSON Schema.
/// Applied as a native per-format constraint (see [`apply_structured`]); the
/// caller decides when to send one (pig-core gates on `cap_structured`).
#[derive(Clone, Debug)]
pub struct StructuredOutput<'a> {
    pub name: &'a str,
    pub schema: serde_json::Value,
}

/// Apply the structured-output constraint to a request body via the format's
/// native parameter — shapes verified against kimi-code's requester shims,
/// ZCode's forced `structuredOutputMode: "outputFormat"`, and codex-rs's
/// TextControls: Chat `response_format.json_schema` (strict), Responses
/// `text.format` json_schema, Anthropic `output_config.format` (nested
/// assignment so a reasoning `output_config.effort` survives). Anthropic has
/// no json_object form (kimi rejects it server-side), so only the schema
/// shape exists.
pub(crate) fn apply_structured(
    body: &mut serde_json::Value,
    format: ApiFormat,
    output: &StructuredOutput<'_>,
) {
    match format {
        ApiFormat::OpenAiChat => {
            body["response_format"] = serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": output.name,
                    "strict": true,
                    "schema": output.schema,
                }
            });
        }
        ApiFormat::OpenAiResponses => {
            body["text"] = serde_json::json!({
                "format": {
                    "type": "json_schema",
                    "strict": true,
                    "name": output.name,
                    "schema": output.schema,
                }
            });
        }
        ApiFormat::AnthropicMessages => {
            body["output_config"]["format"] = serde_json::json!({
                "type": "json_schema",
                "schema": output.schema,
            });
        }
    }
}

/// Non-streaming one-shot request (title generation).
pub async fn complete_text(
    config: &ResolvedModel,
    user_content: String,
    structured: Option<StructuredOutput<'_>>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<CompleteCall, Box<CompleteCallError>> {
    let mut details = CompleteCallDetails::default();
    let result = complete_text_inner(
        config,
        user_content,
        structured,
        cancel,
        &mut details,
        crate::transport::DEFAULT_IDLE_TIMEOUT,
    )
    .await;
    match result {
        // Cancellation that races the last body byte must not report success
        // (or account usage for a call the user already abandoned).
        Ok(_) if cancel.is_cancelled() => {
            details.usage = None;
            Err(Box::new(CompleteCallError {
                error: cancelled_error(),
                usage: None,
                response: details.response,
            }))
        }
        Ok(text) => Ok(CompleteCall {
            text,
            usage: details.usage,
            response: details.response,
        }),
        Err(error) => Err(Box::new(CompleteCallError {
            error,
            usage: details.usage,
            response: details.response,
        })),
    }
}

async fn complete_text_inner(
    config: &ResolvedModel,
    user_content: String,
    structured: Option<StructuredOutput<'_>>,
    cancel: &tokio_util::sync::CancellationToken,
    details: &mut CompleteCallDetails,
    idle: std::time::Duration,
) -> Result<String, CoreError> {
    let client = http_client();
    match config.api_format {
        ApiFormat::OpenAiChat => {
            let mut body = serde_json::json!({
                "model": config.model,
                "messages": [{"role": "user", "content": user_content}],
                "stream": false,
            });
            body[crate::endpoint::chat_token_limit_key(&config.base_url)] =
                serde_json::Value::from(config.max_output_tokens);
            if let Some(output) = &structured {
                apply_structured(&mut body, config.api_format, output);
            }
            let url = crate::endpoint::chat_completions_url(&config.base_url);
            let mut api =
                crate::api_log::ApiCall::new("openai.chat", &config.provider_name, &config.model);
            api.request(&url, &body);
            let response = send_sidecar(
                &mut api,
                |retry| {
                    apply_sdk_headers(
                        client.post(&url).bearer_auth(&config.api_key),
                        false,
                        false,
                        false,
                        retry,
                    )
                    .json(&body)
                },
                cancel,
            )
            .await?;
            details.response = ProviderResponseMetadata::from_response(&response);
            let status = response.status();
            if !status.is_success() {
                let detail = read_sidecar_body(&mut api, response, cancel, idle)
                    .await
                    .error_detail()?;
                let detail = truncated_detail(&detail);
                let error = crate::context_window::http_error(status, &detail);
                api.fail(&core_error_en(&error));
                return Err(error);
            }
            let parsed: CompleteResponse = {
                let raw = read_sidecar_body(&mut api, response, cancel, idle)
                    .await
                    .success(&mut api)?;
                api.raw_line(&raw);
                serde_json::from_str(&raw).map_err(|e| CoreError::Internal {
                    detail: format!("Failed to parse response: {e}"),
                })?
            };
            details.response.response_id = parsed.id.clone().filter(|id| !id.is_empty());
            details.usage = parsed
                .usage
                .as_ref()
                .and_then(|usage| crate::openai::usage_from_json(usage, config.context_window));
            let result = parsed
                .choices
                .and_then(|mut c| c.pop())
                .and_then(|c| c.message)
                .and_then(|m| content_text(m.content.as_ref()))
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
            let mut body = serde_json::json!({
                "model": config.model,
                "max_tokens": config.max_output_tokens,
                "stream": false,
                "messages": [{"role": "user", "content": user_content}],
            });
            if let Some(output) = &structured {
                apply_structured(&mut body, config.api_format, output);
            }
            let url = crate::endpoint::anthropic_messages_url(&config.base_url);
            let mut api = crate::api_log::ApiCall::new(
                "anthropic.messages",
                &config.provider_name,
                &config.model,
            );
            api.request(&url, &body);
            let response = send_sidecar(
                &mut api,
                |retry| {
                    apply_sdk_headers(
                        client
                            .post(&url)
                            .header("x-api-key", &config.api_key)
                            .header("anthropic-version", "2023-06-01"),
                        true,
                        false,
                        false,
                        retry,
                    )
                    .json(&body)
                },
                cancel,
            )
            .await?;
            details.response = ProviderResponseMetadata::from_response(&response);
            let status = response.status();
            if !status.is_success() {
                let detail = read_sidecar_body(&mut api, response, cancel, idle)
                    .await
                    .error_detail()?;
                let detail = truncated_detail(&detail);
                let error = crate::context_window::http_error(status, &detail);
                api.fail(&core_error_en(&error));
                return Err(error);
            }
            let parsed: CompleteResponse = {
                let raw = read_sidecar_body(&mut api, response, cancel, idle)
                    .await
                    .success(&mut api)?;
                api.raw_line(&raw);
                serde_json::from_str(&raw).map_err(|e| CoreError::Internal {
                    detail: format!("Failed to parse response: {e}"),
                })?
            };
            details.response.response_id = parsed.id.clone().filter(|id| !id.is_empty());
            details.usage = parsed
                .usage
                .as_ref()
                .and_then(|usage| crate::anthropic::usage_from_json(usage, config.context_window));
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
            let mut body = serde_json::json!({
                "model": config.model,
                "input": [{"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": user_content}
                ]}],
                "store": false,
                "stream": false,
                "max_output_tokens": config.max_output_tokens,
            });
            if let Some(output) = &structured {
                apply_structured(&mut body, config.api_format, output);
            }
            let url = crate::endpoint::responses_url(&config.base_url);
            let mut api = crate::api_log::ApiCall::new(
                "openai.responses",
                &config.provider_name,
                &config.model,
            );
            api.request(&url, &body);
            let response = send_sidecar(
                &mut api,
                |retry| {
                    apply_sdk_headers(
                        client.post(&url).bearer_auth(&config.api_key),
                        false,
                        false,
                        false,
                        retry,
                    )
                    .json(&body)
                },
                cancel,
            )
            .await?;
            details.response = ProviderResponseMetadata::from_response(&response);
            let status = response.status();
            if !status.is_success() {
                let detail = read_sidecar_body(&mut api, response, cancel, idle)
                    .await
                    .error_detail()?;
                let detail = truncated_detail(&detail);
                let error = crate::context_window::http_error(status, &detail);
                api.fail(&core_error_en(&error));
                return Err(error);
            }
            let raw = read_sidecar_body(&mut api, response, cancel, idle)
                .await
                .success(&mut api)?;
            api.raw_line(&raw);
            let value: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| CoreError::Internal {
                    detail: format!("Failed to parse response: {e}"),
                })?;
            details.response.response_id = value["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            details.usage = value
                .get("usage")
                .and_then(|usage| responses_usage(usage, config.context_window));
            let result = responses_output_text(&value);
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

/// Non-streaming one-shot request (compaction summary), multi-message form:
/// full message list + tool list. The same system/tools/history bytes and the
/// same parallel/reasoning settings as the session request, so OpenAI-family
/// automatic prefix caching hits the cache written by the previous turn. The
/// final user message must be the appended compaction instruction: on
/// Anthropic, the Sidecar cache breakpoint goes on the message before it.
/// A configured `prompt_cache_key` is kept (compact shares the main session's
/// routing key); callers must leave it unset for title/connectivity calls.
/// If the model returns tool calls while summarizing (forbidden by the instructions), treat it as a failure; the caller falls back to the truncation path.
pub async fn complete_messages(
    config: &ResolvedModel,
    messages: &[ChatMsg],
    tools: &[serde_json::Value],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<CompleteCall, Box<CompleteCallError>> {
    let mut details = CompleteCallDetails::default();
    let result = complete_messages_inner(
        config,
        messages,
        tools,
        cancel,
        &mut details,
        crate::transport::DEFAULT_IDLE_TIMEOUT,
    )
    .await;
    match result {
        Ok(_) if cancel.is_cancelled() => {
            details.usage = None;
            Err(Box::new(CompleteCallError {
                error: cancelled_error(),
                usage: None,
                response: details.response,
            }))
        }
        Ok(text) => Ok(CompleteCall {
            text,
            usage: details.usage,
            response: details.response,
        }),
        Err(error) => Err(Box::new(CompleteCallError {
            error,
            usage: details.usage,
            response: details.response,
        })),
    }
}

async fn complete_messages_inner(
    config: &ResolvedModel,
    messages: &[ChatMsg],
    tools: &[serde_json::Value],
    cancel: &tokio_util::sync::CancellationToken,
    details: &mut CompleteCallDetails,
    idle: std::time::Duration,
) -> Result<String, CoreError> {
    let client = http_client();
    match config.api_format {
        ApiFormat::OpenAiChat => {
            // The same builder as the streaming path, so the tool/system bytes
            // and the session cache key align with the previous turn.
            let body = crate::openai::build_chat_body(config, messages, tools, false)?;
            let url = crate::endpoint::chat_completions_url(&config.base_url);
            let mut api =
                crate::api_log::ApiCall::new("openai.chat", &config.provider_name, &config.model);
            api.request(&url, &body);
            let response = send_sidecar(
                &mut api,
                |retry| {
                    apply_sdk_headers(
                        client.post(&url).bearer_auth(&config.api_key),
                        false,
                        false,
                        false,
                        retry,
                    )
                    .json(&body)
                },
                cancel,
            )
            .await?;
            details.response = ProviderResponseMetadata::from_response(&response);
            let status = response.status();
            if !status.is_success() {
                let detail = read_sidecar_body(&mut api, response, cancel, idle)
                    .await
                    .error_detail()?;
                let detail = truncated_detail(&detail);
                let error = crate::context_window::http_error(status, &detail);
                api.fail(&core_error_en(&error));
                return Err(error);
            }
            let parsed: CompleteResponse = {
                let raw = read_sidecar_body(&mut api, response, cancel, idle)
                    .await
                    .success(&mut api)?;
                api.raw_line(&raw);
                serde_json::from_str(&raw).map_err(|e| CoreError::Internal {
                    detail: format!("Failed to parse response: {e}"),
                })?
            };
            details.response.response_id = parsed.id.clone().filter(|id| !id.is_empty());
            details.usage = parsed
                .usage
                .as_ref()
                .and_then(|usage| crate::openai::usage_from_json(usage, config.context_window));
            let result = (|| {
                let message = parsed
                    .choices
                    .and_then(|mut c| c.pop())
                    .and_then(|c| c.message)
                    .ok_or_else(|| CoreError::Internal {
                        detail: "Response has no message".to_string(),
                    })?;
                if message_has_tool_calls(message.tool_calls.as_ref()) {
                    return Err(CoreError::Internal {
                        detail: "Summary response contains tool calls".to_string(),
                    });
                }
                content_text(message.content.as_ref()).ok_or_else(|| CoreError::Internal {
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
            // Sidecar policy keeps the previous turn's tail breakpoint (the
            // last original message) and never marks the appended instruction.
            let body = crate::anthropic::build_anthropic_body(
                config,
                messages,
                tools,
                false,
                crate::anthropic::CachePolicy::Sidecar,
            )?;
            let pdf = messages.iter().any(|message| !message.documents.is_empty());
            let thinking = body.get("thinking").and_then(|t| t.get("type"))
                == Some(&serde_json::json!("enabled"));
            let url = crate::endpoint::anthropic_messages_url(&config.base_url);
            let mut api = crate::api_log::ApiCall::new(
                "anthropic.messages",
                &config.provider_name,
                &config.model,
            );
            api.request(&url, &body);
            let response = send_sidecar(
                &mut api,
                |retry| {
                    apply_sdk_headers(
                        client
                            .post(&url)
                            .header("x-api-key", &config.api_key)
                            .header("anthropic-version", "2023-06-01"),
                        true,
                        thinking,
                        pdf,
                        retry,
                    )
                    .json(&body)
                },
                cancel,
            )
            .await?;
            details.response = ProviderResponseMetadata::from_response(&response);
            let status = response.status();
            if !status.is_success() {
                let detail = read_sidecar_body(&mut api, response, cancel, idle)
                    .await
                    .error_detail()?;
                let detail = truncated_detail(&detail);
                let error = crate::context_window::http_error(status, &detail);
                api.fail(&core_error_en(&error));
                return Err(error);
            }
            let parsed: CompleteResponse = {
                let raw = read_sidecar_body(&mut api, response, cancel, idle)
                    .await
                    .success(&mut api)?;
                api.raw_line(&raw);
                serde_json::from_str(&raw).map_err(|e| CoreError::Internal {
                    detail: format!("Failed to parse response: {e}"),
                })?
            };
            details.response.response_id = parsed.id.clone().filter(|id| !id.is_empty());
            details.usage = parsed
                .usage
                .as_ref()
                .and_then(|usage| crate::anthropic::usage_from_json(usage, config.context_window));
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
            // The same builder as the streaming path, so the input/tool bytes
            // and the session cache key align with the previous turn.
            let body = crate::responses::build_responses_body(config, messages, tools, false)?;
            let url = crate::endpoint::responses_url(&config.base_url);
            let mut api = crate::api_log::ApiCall::new(
                "openai.responses",
                &config.provider_name,
                &config.model,
            );
            api.request(&url, &body);
            let response = send_sidecar(
                &mut api,
                |retry| {
                    apply_sdk_headers(
                        client.post(&url).bearer_auth(&config.api_key),
                        false,
                        false,
                        false,
                        retry,
                    )
                    .json(&body)
                },
                cancel,
            )
            .await?;
            details.response = ProviderResponseMetadata::from_response(&response);
            let status = response.status();
            if !status.is_success() {
                let detail = read_sidecar_body(&mut api, response, cancel, idle)
                    .await
                    .error_detail()?;
                let detail = truncated_detail(&detail);
                let error = crate::context_window::http_error(status, &detail);
                api.fail(&core_error_en(&error));
                return Err(error);
            }
            let raw = read_sidecar_body(&mut api, response, cancel, idle)
                .await
                .success(&mut api)?;
            api.raw_line(&raw);
            let value: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| CoreError::Internal {
                    detail: format!("Failed to parse response: {e}"),
                })?;
            details.response.response_id = value["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            details.usage = value
                .get("usage")
                .and_then(|usage| responses_usage(usage, config.context_window));
            // A function_call item in the summary output is forbidden (same
            // contract as the other formats)
            let result = (|| {
                if value["output"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["type"] == "function_call"))
                {
                    return Err(CoreError::Internal {
                        detail: "Summary response contains tool calls".to_string(),
                    });
                }
                responses_output_text(&value)
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
                let url = crate::endpoint::chat_completions_url(base_url);
                let mut body = serde_json::json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "ping"}],
                    "stream": false,
                });
                body[crate::endpoint::chat_token_limit_key(base_url)] = serde_json::Value::from(1);
                api.request(&url, &body);
                apply_sdk_headers(
                    client.post(&url).bearer_auth(api_key),
                    false,
                    false,
                    false,
                    0,
                )
                .json(&body)
                .send()
                .await
            }
            ApiFormat::AnthropicMessages => {
                let url = crate::endpoint::anthropic_messages_url(base_url);
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
                    false,
                    0,
                )
                .json(&body)
                .send()
                .await
            }
            ApiFormat::OpenAiResponses => {
                let url = crate::endpoint::responses_url(base_url);
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
                apply_sdk_headers(
                    client.post(&url).bearer_auth(api_key),
                    false,
                    false,
                    false,
                    0,
                )
                .json(&body)
                .send()
                .await
            }
        }
    };
    const TIMEOUT_SECS: u64 = 10;
    /// A connectivity probe is a user-facing action, so its error body gets a
    /// short idle deadline and an overall bound instead of the stream-scale
    /// transport default.
    const ERROR_BODY_IDLE: std::time::Duration = std::time::Duration::from_secs(2);
    const ERROR_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    let response =
        match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_SECS), send).await {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                // Network errors fold in the root cause and become the failure detail (same policy as streaming requests)
                let error = send_error(e);
                let detail = match &error {
                    CoreError::Network { detail } => detail.clone(),
                    other => core_error_en(other),
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
        // A stalled error body must not park the probe: read it through the
        // shared idle/cancellation policy under a short overall bound.
        let cancel = tokio_util::sync::CancellationToken::new();
        let detail = match tokio::time::timeout(
            ERROR_BODY_TIMEOUT,
            crate::transport::read_body_with_idle(response, &cancel, ERROR_BODY_IDLE),
        )
        .await
        {
            Ok(Ok(crate::transport::BodyRead::Text(text))) => text,
            Ok(Ok(crate::transport::BodyRead::Cancelled)) => String::new(),
            Ok(Err(failure)) => failure.partial,
            Err(_) => {
                cancel.cancel();
                String::new()
            }
        };
        let detail: String = detail.chars().take(200).collect();
        api.fail(&format!("HTTP {status}: {detail}"));
        ConnTestResult::Failed {
            detail: format!("HTTP {status}: {detail}"),
        }
    }
}

/// Non-streaming Responses reply -> the joined output_text of the message
/// items (reasoning/function_call items are skipped)
fn responses_output_text(value: &serde_json::Value) -> Result<String, CoreError> {
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

#[cfg(test)]
mod tests;
