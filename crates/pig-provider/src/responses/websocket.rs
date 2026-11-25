//! OpenAI Responses WebSocket v2 transport (`ws://` / `wss://`).
//!
//! Implements the Codex-style Responses WebSocket v2 protocol:
//! - Persistent connection with `OpenAI-Beta: responses_websockets=2026-02-06`
//! - `response.create` client events with `previous_response_id` incremental
//!   continuation and `generate: false` session prewarm
//! - Server event frames routed directly through [`super::stream::handle_frame`]
//!   and [`super::stream::finish_responses_stream`] so text, reasoning, tool
//!   calls, citations, usage, and metadata semantics stay identical to HTTP/SSE.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};

use futures_util::{SinkExt as _, StreamExt as _};
use pig_protocol::CoreError;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::api_log;
use crate::chat::{ChatMsg, ResolvedModel};
use crate::events::{CallControl, CallError, ProviderEvent};

use super::live::LiveResponsesText;
use super::request::{build_responses_body, to_responses_input};
use super::stream::{finish_responses_stream, handle_frame};

/// OpenAI Responses WebSocket v2 beta header (aligned with Codex `codex-api`).
pub const RESPONSES_WS_BETA_HEADER: &str = "responses_websockets=2026-02-06";

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn hash_messages(messages: &[ChatMsg]) -> u64 {
    let mut hasher = DefaultHasher::new();
    messages.len().hash(&mut hasher);
    for msg in messages {
        if let Ok(encoded) = serde_json::to_string(msg) {
            encoded.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn hash_turn_config(
    config: &ResolvedModel,
    messages: &[ChatMsg],
    tools: &[serde_json::Value],
    structured: Option<&crate::StructuredOutput<'_>>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    config.base_url.hash(&mut hasher);
    config.model.hash(&mut hasher);
    if let Some(params) = &config.reasoning_params
        && let Ok(encoded) = serde_json::to_string(params)
    {
        encoded.hash(&mut hasher);
    }
    config.prompt_cache_key.hash(&mut hasher);
    config.cap_web_search.hash(&mut hasher);
    config.tool_settings.parallel_tool_calls.hash(&mut hasher);
    // System messages hoist into top-level `instructions`; any system-prompt change breaks chain continuity.
    for msg in messages.iter().filter(|m| m.role == "system") {
        msg.content.hash(&mut hasher);
    }
    if let Ok(encoded) = serde_json::to_string(tools) {
        encoded.hash(&mut hasher);
    }
    if let Some(output) = structured {
        output.name.hash(&mut hasher);
        if let Ok(encoded) = serde_json::to_string(&output.schema) {
            encoded.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Build the WebSocket endpoint URL from `config.base_url`.
/// Converts `http://` -> `ws://`, `https://` -> `wss://`, keeps `ws://`/`wss://`,
/// and appends `/responses` if not already present.
pub fn responses_ws_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let ws_base = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        trimmed.to_string()
    };
    if ws_base.ends_with("/responses") {
        ws_base
    } else {
        format!("{ws_base}/responses")
    }
}

/// Build the `response.create` client WebSocket payload.
///
/// When `previous_response_id` and `skip_messages` are supplied, `instructions`
/// and request metadata are still derived from the full conversation while
/// `input` carries only the newly appended messages `&messages[skip_messages..]`.
fn build_ws_create_event(
    config: &ResolvedModel,
    messages: &[ChatMsg],
    tools: &[serde_json::Value],
    structured: Option<&crate::StructuredOutput<'_>>,
    previous_response_id: Option<&str>,
    skip_messages: Option<usize>,
    prewarm: bool,
) -> Result<serde_json::Value, CallError> {
    let mut body = build_responses_body(config, messages, tools, true)?;
    if let Some(output) = structured {
        crate::sidecar::apply_structured(&mut body, config.api_format, output);
    }
    if let serde_json::Value::Object(map) = &mut body {
        // WebSocket v2 frames use `type: "response.create"` instead of `stream: true`.
        map.remove("stream");
        map.insert(
            "type".to_string(),
            serde_json::Value::String("response.create".to_string()),
        );
        if prewarm {
            map.insert("generate".to_string(), serde_json::Value::Bool(false));
        }
        if let Some(prev_id) = previous_response_id {
            map.insert(
                "previous_response_id".to_string(),
                serde_json::Value::String(prev_id.to_string()),
            );
            // When continuing an existing server-side response chain, send only
            // the newly appended input items after the server's last response.
            if let Some(skip) = skip_messages
                && skip < messages.len()
            {
                let (_, suffix_input) = to_responses_input(config, &messages[skip..]);
                map.insert("input".to_string(), serde_json::Value::Array(suffix_input));
            }
        }
    }
    Ok(body)
}

async fn connect_ws(config: &ResolvedModel, control: &CallControl) -> Result<WsStream, CallError> {
    if control.is_cancelled() {
        return Err(CoreError::Internal {
            detail: "WebSocket connect cancelled".to_string(),
        }
        .into());
    }
    let url = responses_ws_url(&config.base_url);
    let mut request = url.as_str().into_client_request().map_err(|e| {
        CallError::from(CoreError::Internal {
            detail: format!("Invalid Responses WebSocket URL `{url}`: {e}"),
        })
    })?;
    let headers = request.headers_mut();
    headers.insert(
        "openai-beta",
        RESPONSES_WS_BETA_HEADER
            .parse()
            .expect("valid beta header"),
    );
    if !config.api_key.is_empty()
        && let Ok(val) = format!("Bearer {}", config.api_key).parse()
    {
        headers.insert("authorization", val);
    }

    let connect_fut = connect_async(request);
    let outcome = tokio::select! {
        biased;
        _ = control.cancelled() => {
            return Err(CoreError::Internal {
                detail: "WebSocket connect cancelled".to_string(),
            }.into());
        }
        res = tokio::time::timeout(std::time::Duration::from_secs(15), connect_fut) => res,
    };
    match outcome {
        Ok(Ok((ws, _response))) => Ok(ws),
        Ok(Err(error)) => Err(CoreError::Network {
            detail: format!("Responses WebSocket connect failed: {error}"),
        }
        .into()),
        Err(_) => Err(CoreError::Network {
            detail: "Responses WebSocket connect timed out after 15s".to_string(),
        }
        .into()),
    }
}

/// Persistent Responses WebSocket v2 session supporting connection reuse,
/// `previous_response_id` incremental continuation, and `generate: false` prewarm.
pub struct ResponsesWsSession {
    socket: Option<WsStream>,
    previous_response_id: Option<String>,
    last_messages_len: usize,
    last_turn_was_generated: bool,
    last_prefix_hash: Option<u64>,
    last_config_hash: Option<u64>,
}

impl Default for ResponsesWsSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesWsSession {
    pub fn new() -> Self {
        Self {
            socket: None,
            previous_response_id: None,
            last_messages_len: 0,
            last_turn_was_generated: false,
            last_prefix_hash: None,
            last_config_hash: None,
        }
    }

    /// The server response ID from the last completed turn or prewarm.
    pub fn previous_response_id(&self) -> Option<&str> {
        self.previous_response_id.as_deref()
    }

    /// Clear continuation state (for example after history compaction or fork).
    pub fn reset_chain(&mut self) {
        self.previous_response_id = None;
        self.last_messages_len = 0;
        self.last_turn_was_generated = false;
        self.last_prefix_hash = None;
        self.last_config_hash = None;
    }

    /// Resolve whether `messages` is a strict continuation of the previous
    /// turn/prewarm on the same endpoint/config. Returns `(previous_response_id,
    /// skip_messages)` when incremental continuation is safe, or `(None, None)`
    /// after resetting stale chain state.
    fn resolve_continuation(
        &mut self,
        config_hash: u64,
        messages: &[ChatMsg],
    ) -> (Option<String>, Option<usize>) {
        let Some(prev_id) = self.previous_response_id.clone() else {
            return (None, None);
        };
        if self.last_config_hash != Some(config_hash) {
            self.reset_chain();
            return (None, None);
        }
        if self.last_messages_len == 0 || messages.len() <= self.last_messages_len {
            self.reset_chain();
            return (None, None);
        }
        if self.last_prefix_hash != Some(hash_messages(&messages[..self.last_messages_len])) {
            self.reset_chain();
            return (None, None);
        }
        let skip_messages = if self.last_turn_was_generated {
            // The server already owns the assistant output produced by `prev_id`.
            // Skip that assistant message and transmit only subsequent user/tool messages.
            if messages
                .get(self.last_messages_len)
                .is_some_and(|m| m.role == "assistant")
            {
                self.last_messages_len + 1
            } else {
                self.reset_chain();
                return (None, None);
            }
        } else {
            self.last_messages_len
        };
        if skip_messages >= messages.len() {
            self.reset_chain();
            return (None, None);
        }
        (Some(prev_id), Some(skip_messages))
    }

    /// Prewarm the server-side prompt cache (`generate: false`) so the first
    /// real turn can continue from the returned `previous_response_id`.
    pub async fn prewarm(
        &mut self,
        config: &ResolvedModel,
        messages: &[ChatMsg],
        tools: &[serde_json::Value],
        control: &CallControl,
    ) -> Result<Option<String>, CallError> {
        if self.socket.is_none() {
            self.socket = Some(connect_ws(config, control).await?);
        }
        let config_hash = hash_turn_config(config, messages, tools, None);
        let payload = build_ws_create_event(config, messages, tools, None, None, None, true)?;
        let ws = self.socket.as_mut().expect("connected ws");
        if let Err(error) = ws.send(Message::Text(payload.to_string().into())).await {
            self.socket = None;
            return Err(CoreError::Network {
                detail: format!("Responses WebSocket prewarm send failed: {error}"),
            }
            .into());
        }

        let idle = control.idle_timeout();
        let mut response_id: Option<String> = None;
        loop {
            let next = tokio::select! {
                biased;
                _ = control.cancelled() => return Ok(response_id),
                msg = tokio::time::timeout(idle, ws.next()) => match msg {
                    Ok(msg) => msg,
                    Err(_) => {
                        self.socket = None;
                        return Err(CoreError::StreamRead {
                            detail: crate::transport::idle_detail("Responses WebSocket prewarm", idle),
                        }.into());
                    }
                }
            };
            match next {
                Some(Ok(Message::Text(text))) => {
                    let value: serde_json::Value =
                        serde_json::from_str(&text).map_err(|e| {
                            CallError::from(CoreError::Internal {
                                detail: format!("Invalid JSON in Responses WS prewarm: {e}"),
                            })
                        })?;
                    if let Some(id) = value["response"]["id"].as_str().filter(|s| !s.is_empty()) {
                        response_id = Some(id.to_string());
                    }
                    match value["type"].as_str().unwrap_or("") {
                        "response.completed" | "response.incomplete" => {
                            self.previous_response_id = response_id.clone();
                            self.last_messages_len = messages.len();
                            self.last_turn_was_generated = false;
                            self.last_prefix_hash = Some(hash_messages(messages));
                            self.last_config_hash = Some(config_hash);
                            return Ok(response_id);
                        }
                        "response.failed" | "error" => {
                            let detail = value["error"]["message"]
                                .as_str()
                                .or_else(|| value["response"]["error"]["message"].as_str())
                                .unwrap_or("Responses WS prewarm failed")
                                .to_string();
                            return Err(CoreError::Internal { detail }.into());
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    let _ = ws.send(Message::Pong(data)).await;
                }
                Some(Ok(Message::Close(_))) | None => {
                    self.socket = None;
                    break;
                }
                Some(Err(error)) => {
                    self.socket = None;
                    return Err(CoreError::StreamRead {
                        detail: error.to_string(),
                    }
                    .into());
                }
                _ => {}
            }
        }
        Ok(response_id)
    }

    /// Stream one turn over the persistent WebSocket connection, continuing from
    /// [`Self::previous_response_id`] when available.
    pub async fn stream_turn(
        &mut self,
        config: &ResolvedModel,
        messages: Vec<ChatMsg>,
        tools: Vec<serde_json::Value>,
        tx: &tokio::sync::mpsc::Sender<ProviderEvent>,
        control: &CallControl,
    ) -> Result<(), CallError> {
        let mut reconnected = false;
        let mut retried_without_chain = false;
        let config_hash = hash_turn_config(config, &messages, &tools, control.structured_output());
        loop {
            if self.socket.is_none() {
                self.socket = Some(connect_ws(config, control).await?);
            }
            let (prev_id, skip_messages) = self.resolve_continuation(config_hash, &messages);
            let used_chain = prev_id.is_some();
            let payload = build_ws_create_event(
                config,
                &messages,
                &tools,
                control.structured_output(),
                prev_id.as_deref(),
                skip_messages,
                false,
            )?;

            let ws = self.socket.as_mut().expect("connected ws");
            if let Err(error) = ws.send(Message::Text(payload.to_string().into())).await {
                self.socket = None;
                if !reconnected && !control.is_cancelled() {
                    reconnected = true;
                    continue;
                }
                return Err(CoreError::Network {
                    detail: format!("Responses WebSocket send failed: {error}"),
                }
                .into());
            }

            let mut api = api_log::ApiCall::new(
                "openai.responses.ws",
                &config.provider_name,
                &config.model,
            );
            api.request(&responses_ws_url(&config.base_url), &payload);
            let mut response_metadata = crate::ProviderResponseMetadata::default();
            let mut tool_stream = crate::tool_stream::ToolCallStream::new();
            let mut replay_items = Vec::new();
            let mut live = LiveResponsesText::default();
            let mut finish_reason = None;
            let mut completed_output = None;
            let idle = control.idle_timeout();
            let mut frame_error: Option<CallError> = None;

            loop {
                let msg = tokio::select! {
                    biased;
                    _ = control.cancelled() => {
                        api.finish("cancelled", tool_stream.calls());
                        // Drop socket on mid-turn cancel so residual frames
                        // cannot corrupt the next turn.
                        self.socket = None;
                        return Ok(());
                    }
                    msg = tokio::time::timeout(idle, ws.next()) => match msg {
                        Ok(msg) => msg,
                        Err(_) => {
                            let detail = crate::transport::idle_detail("WebSocket stream", idle);
                            api.fail(&detail);
                            self.socket = None;
                            return Err(CoreError::StreamRead { detail }.into());
                        }
                    }
                };
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        api.raw_line(&text);
                        match handle_frame(
                            text.trim(),
                            tx,
                            &mut tool_stream,
                            &mut replay_items,
                            &mut live,
                            &mut finish_reason,
                            config,
                            &mut api,
                            &mut response_metadata,
                            &mut completed_output,
                        )
                        .await
                        {
                            Ok(true) => {
                                if control.is_cancelled() {
                                    api.finish("cancelled", tool_stream.calls());
                                    self.socket = None;
                                    return Ok(());
                                }
                                if let Some(id) = response_metadata.response_id.clone() {
                                    self.previous_response_id = Some(id);
                                    self.last_messages_len = messages.len();
                                    self.last_turn_was_generated = true;
                                    self.last_prefix_hash = Some(hash_messages(&messages));
                                    self.last_config_hash = Some(config_hash);
                                }
                                return finish_responses_stream(
                                    tool_stream,
                                    replay_items,
                                    live,
                                    finish_reason,
                                    completed_output,
                                    config,
                                    &mut api,
                                    tx,
                                )
                                .await;
                            }
                            Ok(false) => {}
                            Err(err) => {
                                frame_error = Some(err);
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        self.socket = None;
                        break;
                    }
                    Some(Err(error)) => {
                        self.socket = None;
                        return Err(CoreError::StreamRead {
                            detail: error.to_string(),
                        }
                        .into());
                    }
                    _ => {}
                }
            }

            if let Some(err) = frame_error {
                // If an incremental `previous_response_id` request failed before
                // emitting any content (e.g., server evicted the cached response),
                // reset the chain and retry once with full conversation input.
                let emitted_anything =
                    !live.is_empty() || !tool_stream.calls().is_empty() || !replay_items.is_empty();
                if used_chain && !retried_without_chain && !emitted_anything && !control.is_cancelled() {
                    tracing::warn!(
                        "Responses WS incremental turn failed before output; resetting chain and retrying with full input"
                    );
                    self.reset_chain();
                    retried_without_chain = true;
                    continue;
                }
                return Err(err);
            }

            if control.is_cancelled() {
                api.finish("cancelled", tool_stream.calls());
                return Ok(());
            }
            if let Some(id) = response_metadata.response_id.clone() {
                self.previous_response_id = Some(id);
                self.last_messages_len = messages.len();
                self.last_turn_was_generated = true;
                self.last_prefix_hash = Some(hash_messages(&messages));
                self.last_config_hash = Some(config_hash);
            }
            return finish_responses_stream(
                tool_stream,
                replay_items,
                live,
                finish_reason,
                completed_output,
                config,
                &mut api,
                tx,
            )
            .await;
        }
    }

    /// Gracefully close the underlying WebSocket connection.
    pub async fn close(&mut self) {
        if let Some(mut ws) = self.socket.take() {
            let _ = ws.close(None).await;
        }
    }
}

/// Stateless one-shot Responses WebSocket v2 streaming call used by
/// [`crate::registry::OpenAiResponsesWsDriver`].
pub(crate) async fn stream_responses_ws(
    config: &ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    previous_response_id: Option<String>,
    tx: &tokio::sync::mpsc::Sender<ProviderEvent>,
    control: &CallControl,
) -> Result<(), CallError> {
    let mut session = ResponsesWsSession::new();
    session.previous_response_id = previous_response_id;
    let result = session.stream_turn(config, messages, tools, tx, control).await;
    session.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::test_model;
    use pig_protocol::{ApiFormat, FinishReason};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    #[test]
    fn responses_ws_url_converts_schemes_and_appends_path() {
        assert_eq!(
            responses_ws_url("https://api.openai.com/v1"),
            "wss://api.openai.com/v1/responses"
        );
        assert_eq!(
            responses_ws_url("http://127.0.0.1:8080/v1/"),
            "ws://127.0.0.1:8080/v1/responses"
        );
        assert_eq!(
            responses_ws_url("wss://api.openai.com/v1/responses"),
            "wss://api.openai.com/v1/responses"
        );
    }

    #[tokio::test]
    async fn responses_ws_v2_supports_prewarm_previous_response_id_and_registry_dispatch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let received_payloads: Arc<Mutex<Vec<serde_json::Value>>> =
            Arc::new(Mutex::new(Vec::new()));
        let seen_headers: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

        let payloads_server = Arc::clone(&received_payloads);
        let headers_server = Arc::clone(&seen_headers);
        tokio::spawn(async move {
            let mut turn_counter = 0usize;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let payloads = Arc::clone(&payloads_server);
                let headers = Arc::clone(&headers_server);
                tokio::spawn(async move {
                    let callback = |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                                    resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                        let mut h = headers.lock().unwrap();
                        for (k, v) in req.headers() {
                            if let Ok(val) = v.to_str() {
                                h.push((k.as_str().to_ascii_lowercase(), val.to_string()));
                            }
                        }
                        Ok(resp)
                    };
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await
                    else {
                        return;
                    };
                    while let Some(Ok(Message::Text(text))) = ws.next().await {
                        let Ok(req) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        payloads.lock().unwrap().push(req.clone());
                        turn_counter += 1;
                        let resp_id = format!("resp_ws_{turn_counter}");
                        if req["generate"].as_bool() == Some(false) {
                            let done = serde_json::json!({
                                "type": "response.completed",
                                "response": { "id": resp_id, "status": "completed", "output": [] }
                            });
                            let _ = ws.send(Message::Text(done.to_string().into())).await;
                            continue;
                        }
                        let created = serde_json::json!({
                            "type": "response.created",
                            "response": { "id": resp_id }
                        });
                        let delta = serde_json::json!({
                            "type": "response.output_text.delta",
                            "output_index": 0,
                            "item_id": "msg_1",
                            "delta": format!("ws-reply-{turn_counter}")
                        });
                        let completed = serde_json::json!({
                            "type": "response.completed",
                            "response": {
                                "id": resp_id,
                                "status": "completed",
                                "output": [{
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{ "type": "output_text", "text": format!("ws-reply-{turn_counter}") }]
                                }],
                                "usage": { "input_tokens": 10, "output_tokens": 4, "total_tokens": 14 }
                            }
                        });
                        let _ = ws.send(Message::Text(created.to_string().into())).await;
                        let _ = ws.send(Message::Text(delta.to_string().into())).await;
                        let _ = ws.send(Message::Text(completed.to_string().into())).await;
                    }
                });
            }
        });

        let mut config = test_model(ApiFormat::OpenAiResponses);
        config.base_url = format!("ws://{addr}/v1");
        config.api_key = "sk-ws-test".into();

        let control = CallControl::new(tokio_util::sync::CancellationToken::new());
        let mut session = ResponsesWsSession::new();

        // 1. Prewarm (generate: false)
        let msg1 = vec![ChatMsg::user("hello".to_string())];
        let warm_id = session
            .prewarm(&config, &msg1, &[], &control)
            .await
            .expect("prewarm");
        assert!(warm_id.is_some());
        assert_eq!(session.previous_response_id(), warm_id.as_deref());

        // 2. Turn 1 over the same socket -> carries previous_response_id + incremental input
        let msg2 = vec![
            ChatMsg::user("hello".to_string()),
            ChatMsg::user("second turn".to_string()),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        session
            .stream_turn(&config, msg2, vec![], &tx, &control)
            .await
            .expect("turn 1");
        drop(tx);
        let mut text = String::new();
        let mut finished = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                ProviderEvent::Text(t) => text.push_str(&t),
                ProviderEvent::Finished(FinishReason::Stop) => finished = true,
                _ => {}
            }
        }
        assert!(text.contains("ws-reply-"), "{text}");
        assert!(finished);
        session.close().await;

        // Verify wire contract: beta header, auth header, prewarm generate=false, and incremental input.
        {
            let hdrs = seen_headers.lock().unwrap();
            assert!(
                hdrs.iter()
                    .any(|(k, v)| k == "openai-beta" && v == RESPONSES_WS_BETA_HEADER),
                "{hdrs:?}"
            );
            assert!(
                hdrs.iter()
                    .any(|(k, v)| k == "authorization" && v == "Bearer sk-ws-test"),
                "{hdrs:?}"
            );
        }
        {
            let reqs = received_payloads.lock().unwrap();
            assert_eq!(reqs.len(), 2);
            assert_eq!(reqs[0]["type"], "response.create");
            assert_eq!(reqs[0]["generate"], false);
            assert_eq!(reqs[1]["type"], "response.create");
            assert_eq!(reqs[1]["previous_response_id"], warm_id.unwrap());
            assert_eq!(
                reqs[1]["input"].as_array().unwrap().len(),
                1,
                "incremental continuation should send only the new turn input"
            );
        }

        // 3. Stateless dispatch via global ProviderRegistry + stream_chat
        let (tx3, mut rx3) = tokio::sync::mpsc::channel(32);
        crate::stream_chat(
            config,
            vec![ChatMsg::user("stateless".to_string())],
            vec![],
            tx3,
            control,
        )
        .await;
        let mut text3 = String::new();
        while let Some(ev) = rx3.recv().await {
            if let ProviderEvent::Text(t) = ev {
                text3.push_str(&t);
            }
        }
        assert!(text3.contains("ws-reply-"), "{text3}");
    }
}
