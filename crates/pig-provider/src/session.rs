//! Protocol-agnostic session-scoped provider transport.
//!
//! Owns persistent connection state (such as OpenAI Responses WebSocket v2
//! sockets and `previous_response_id` continuation chains) behind an opaque
//! handle so `pig-core`'s session layer never needs `ApiFormat` or provider
//! special cases.
//!
//! When a Responses endpoint supports WebSocket v2 (`ws://`/`wss://`, official
//! `api.openai.com`, or opt-in via `PIG_RESPONSES_WS=1`), turns stream over the
//! persistent WebSocket and continue incrementally. If an `http://`/`https://`
//! endpoint fails over WebSocket before emitting any visible content (for
//! example behind an HTTP-only corporate proxy or compatible gateway), the
//! session records the fallback and transparently retries over HTTP/SSE.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pig_protocol::{ApiFormat, FinishReason};

use crate::chat::{ChatMsg, ResolvedModel};
use crate::core_error_en;
use crate::endpoint::responses_ws_supported;
use crate::events::{CallControl, ProviderEvent, RetryNotice, RetryReason};
use crate::responses::ResponsesWsSession;

/// Process-wide cache of normalized `base_url`s that rejected Responses WS v2
/// upgrade so subsequent sessions skip the handshake penalty immediately.
fn global_ws_fallback_hosts() -> &'static Mutex<HashSet<String>> {
    static HOSTS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    HOSTS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn normalize_base_url(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_ascii_lowercase()
}

fn env_prefers_responses_ws() -> bool {
    std::env::var("PIG_RESPONSES_WS")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

struct ProviderSessionInner {
    ws: ResponsesWsSession,
    observed_reset_gen: u64,
    active_endpoint: Option<(String, String, ApiFormat)>,
    ws_fallback_urls: HashSet<String>,
    prefer_ws_on_http: bool,
}

impl ProviderSessionInner {
    fn new(prefer_ws_on_http: bool) -> Self {
        Self {
            ws: ResponsesWsSession::new(),
            observed_reset_gen: 0,
            active_endpoint: None,
            ws_fallback_urls: HashSet::new(),
            prefer_ws_on_http,
        }
    }

    fn is_ws_fallback(&self, base_url: &str) -> bool {
        let key = normalize_base_url(base_url);
        if self.ws_fallback_urls.contains(&key) {
            return true;
        }
        global_ws_fallback_hosts()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&key)
    }

    fn mark_ws_fallback(&mut self, base_url: &str) {
        let key = normalize_base_url(base_url);
        self.ws_fallback_urls.insert(key.clone());
        global_ws_fallback_hosts()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key);
    }

    fn should_use_ws(&self, config: &ResolvedModel) -> bool {
        if config.api_format != ApiFormat::OpenAiResponses {
            return false;
        }
        if config.base_url.starts_with("ws://") || config.base_url.starts_with("wss://") {
            return true;
        }
        let eligible =
            responses_ws_supported(&config.base_url) || self.prefer_ws_on_http || env_prefers_responses_ws();
        eligible && !self.is_ws_fallback(&config.base_url)
    }
}

/// Session-scoped provider transport handle (cheap to clone across spawned turn
/// tasks; `reset()` is synchronous and lock-free).
#[derive(Clone)]
pub struct ProviderSession {
    inner: Arc<tokio::sync::Mutex<ProviderSessionInner>>,
    reset_gen: Arc<AtomicU64>,
}

impl Default for ProviderSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderSession {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(ProviderSessionInner::new(false))),
            reset_gen: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Opt in to attempting Responses WS v2 upgrade even on non-official
    /// `http://` / `https://` endpoints (falling back to HTTP/SSE automatically
    /// if the server does not accept WebSocket upgrades).
    pub fn with_prefer_ws_on_http(prefer: bool) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(ProviderSessionInner::new(prefer))),
            reset_gen: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Invalidate any server-side continuation state (`previous_response_id`).
    /// Call whenever conversation history is modified non-incrementally
    /// (compaction, recall, fork, media reprojection).
    pub fn reset(&self) {
        self.reset_gen.fetch_add(1, Ordering::Release);
    }

    /// Best-effort WebSocket prompt-cache prewarm (`generate: false`).
    /// No-op for formats/endpoints that do not use Responses WS v2.
    pub async fn prewarm(
        &self,
        config: &ResolvedModel,
        messages: &[ChatMsg],
        tools: &[serde_json::Value],
        control: &CallControl,
    ) {
        let mut inner = self.inner.lock().await;
        let current_gen = self.reset_gen.load(Ordering::Acquire);
        if inner.observed_reset_gen != current_gen {
            inner.ws.reset_chain();
            inner.ws.close().await;
            inner.observed_reset_gen = current_gen;
        }
        if !inner.should_use_ws(config) {
            return;
        }
        if let Err(err) = inner.ws.prewarm(config, messages, tools, control).await {
            tracing::debug!(
                base_url = %config.base_url,
                error = %core_error_en(&err.error),
                "Responses WS prewarm failed; ignoring"
            );
            inner.ws.reset_chain();
            inner.ws.close().await;
        }
    }

    /// Stream one model turn, reusing persistent Responses WebSocket v2 state
    /// when applicable and transparently falling back to HTTP/SSE when an
    /// `http://`/`https://` endpoint fails before emitting visible output.
    pub async fn stream_turn(
        &self,
        config: ResolvedModel,
        messages: Vec<ChatMsg>,
        tools: Vec<serde_json::Value>,
        mut tx: tokio::sync::mpsc::Sender<ProviderEvent>,
        control: CallControl,
    ) {
        let mut inner = self.inner.lock().await;
        let current_gen = self.reset_gen.load(Ordering::Acquire);
        if inner.observed_reset_gen != current_gen {
            inner.ws.reset_chain();
            inner.ws.close().await;
            inner.observed_reset_gen = current_gen;
        }

        let endpoint = (
            config.base_url.clone(),
            config.model.clone(),
            config.api_format,
        );
        if inner.active_endpoint.as_ref() != Some(&endpoint) {
            inner.ws.reset_chain();
            inner.ws.close().await;
            inner.active_endpoint = Some(endpoint);
        }

        if !inner.should_use_ws(&config) {
            drop(inner);
            crate::stream_chat(config, messages, tools, tx, control).await;
            return;
        }

        let is_explicit_ws =
            config.base_url.starts_with("ws://") || config.base_url.starts_with("wss://");
        let mut retried_empty = false;

        loop {
            let (itx, mut irx) = tokio::sync::mpsc::channel::<ProviderEvent>(256);
            let saw_content = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&saw_content);
            let forward = tokio::spawn(async move {
                let mut held_finish = None;
                let mut held_usage = None;
                while let Some(event) = irx.recv().await {
                    match &event {
                        ProviderEvent::Finished(reason) => {
                            held_finish = Some(*reason);
                            continue;
                        }
                        ProviderEvent::Usage { .. } => {
                            held_usage = Some(event);
                            continue;
                        }
                        _ => {}
                    }
                    let has_content = match &event {
                        ProviderEvent::Text(_)
                        | ProviderEvent::Reasoning(_)
                        | ProviderEvent::ToolCalls(_) => true,
                        ProviderEvent::AssistantReplay(replay) => !replay.items.is_empty(),
                        _ => false,
                    };
                    if has_content {
                        flag.store(true, Ordering::Relaxed);
                    }
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
                (tx, held_finish, held_usage)
            });

            let result = inner
                .ws
                .stream_turn(&config, messages.clone(), tools.clone(), &itx, &control)
                .await;
            drop(itx);
            let Ok((back, held_finish, held_usage)) = forward.await else {
                return;
            };
            tx = back;

            match result {
                Ok(()) => {
                    let empty = !saw_content.load(Ordering::Relaxed);
                    if empty
                        && matches!(held_finish, Some(FinishReason::Stop))
                        && !retried_empty
                        && !control.is_cancelled()
                    {
                        retried_empty = true;
                        tracing::warn!(
                            "Responses WS stream completed empty (no text/reasoning/tool calls), retrying once"
                        );
                        let _ = tx
                            .send(ProviderEvent::Retrying(RetryNotice {
                                attempt: 1,
                                max_attempts: 1,
                                delay: Duration::ZERO,
                                reason: RetryReason::EmptyCompletion,
                            }))
                            .await;
                        continue;
                    }
                    if let Some(usage) = held_usage {
                        let _ = tx.send(usage).await;
                    }
                    if let Some(reason) = held_finish {
                        let _ = tx.send(ProviderEvent::Finished(reason)).await;
                    }
                    return;
                }
                Err(call_error) => {
                    let emitted_visible = saw_content.load(Ordering::Relaxed);
                    if !is_explicit_ws && !emitted_visible && !control.is_cancelled() {
                        tracing::warn!(
                            base_url = %config.base_url,
                            error = %core_error_en(&call_error.error),
                            "Responses WS v2 failed before output; falling back to HTTP/SSE"
                        );
                        inner.mark_ws_fallback(&config.base_url);
                        inner.ws.reset_chain();
                        inner.ws.close().await;
                        drop(inner);
                        crate::stream_chat(config, messages, tools, tx, control).await;
                        return;
                    }
                    if let Some(usage) = held_usage {
                        let _ = tx.send(usage).await;
                    }
                    let _ = tx.send(ProviderEvent::Failed(call_error.error)).await;
                    return;
                }
            }
        }
    }

    /// Gracefully close any underlying persistent connection.
    pub async fn close(&self) {
        let mut inner = self.inner.lock().await;
        inner.ws.reset_chain();
        inner.ws.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::test_model;
    use futures_util::{SinkExt as _, StreamExt as _};
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test]
    async fn provider_session_reuses_ws_sends_incremental_input_and_resets_chain() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let connections = Arc::new(AtomicUsize::new(0));
        let received_payloads: Arc<Mutex<Vec<serde_json::Value>>> =
            Arc::new(Mutex::new(Vec::new()));

        let conn_srv = Arc::clone(&connections);
        let payloads_srv = Arc::clone(&received_payloads);
        let turn_counter = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                conn_srv.fetch_add(1, Ordering::SeqCst);
                let payloads = Arc::clone(&payloads_srv);
                let turns = Arc::clone(&turn_counter);
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(Message::Text(text))) = ws.next().await {
                        let Ok(req) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        payloads.lock().unwrap().push(req);
                        let n = turns.fetch_add(1, Ordering::SeqCst) + 1;
                        let resp_id = format!("resp_chain_{n}");
                        let reply_text = format!("reply-{n}");
                        let created = serde_json::json!({
                            "type": "response.created",
                            "response": { "id": resp_id }
                        });
                        let delta = serde_json::json!({
                            "type": "response.output_text.delta",
                            "output_index": 0,
                            "item_id": format!("msg_{n}"),
                            "delta": reply_text
                        });
                        let completed = serde_json::json!({
                            "type": "response.completed",
                            "response": {
                                "id": resp_id,
                                "status": "completed",
                                "output": [{
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{ "type": "output_text", "text": reply_text }]
                                }],
                                "usage": { "input_tokens": 12, "output_tokens": 5, "total_tokens": 17 }
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
        config.api_key = "sk-test".into();

        let session = ProviderSession::new();
        let control = CallControl::new(tokio_util::sync::CancellationToken::new());

        // Turn 1: full history [system, user1]
        let turn1_msgs = vec![
            ChatMsg::system("sys prompt".into()),
            ChatMsg::user("first question".into()),
        ];
        let (tx1, mut rx1) = tokio::sync::mpsc::channel(32);
        session
            .stream_turn(config.clone(), turn1_msgs.clone(), vec![], tx1, control.clone())
            .await;
        let mut text1 = String::new();
        while let Some(ev) = rx1.recv().await {
            if let ProviderEvent::Text(t) = ev {
                text1.push_str(&t);
            }
        }
        assert_eq!(text1, "reply-1");

        // Turn 2: [system, user1, assistant(reply-1), user2] -> must reuse socket,
        // carry previous_response_id = resp_chain_1, and send ONLY [user2] (skipping assistant).
        let mut turn2_msgs = turn1_msgs.clone();
        turn2_msgs.push(ChatMsg::assistant("reply-1".into(), vec![], None));
        turn2_msgs.push(ChatMsg::user("second question".into()));
        let (tx2, mut rx2) = tokio::sync::mpsc::channel(32);
        session
            .stream_turn(config.clone(), turn2_msgs.clone(), vec![], tx2, control.clone())
            .await;
        let mut text2 = String::new();
        while let Some(ev) = rx2.recv().await {
            if let ProviderEvent::Text(t) = ev {
                text2.push_str(&t);
            }
        }
        assert_eq!(text2, "reply-2");

        // Reset (e.g. after compaction/recall): Turn 3 must NOT carry previous_response_id.
        session.reset();
        let turn3_msgs = vec![
            ChatMsg::system("sys prompt".into()),
            ChatMsg::user("compacted summary + new prompt".into()),
        ];
        let (tx3, mut rx3) = tokio::sync::mpsc::channel(32);
        session
            .stream_turn(config.clone(), turn3_msgs, vec![], tx3, control)
            .await;
        let mut text3 = String::new();
        while let Some(ev) = rx3.recv().await {
            if let ProviderEvent::Text(t) = ev {
                text3.push_str(&t);
            }
        }
        assert_eq!(text3, "reply-3");
        session.close().await;

        let reqs = received_payloads.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        assert!(reqs[0].get("previous_response_id").is_none());
        assert_eq!(reqs[0]["input"].as_array().unwrap().len(), 1);

        assert_eq!(reqs[1]["previous_response_id"], "resp_chain_1");
        let turn2_input = reqs[1]["input"].as_array().unwrap();
        assert_eq!(
            turn2_input.len(),
            1,
            "incremental turn must skip the server-owned assistant message and send only user2: {turn2_input:?}"
        );
        assert_eq!(turn2_input[0]["role"], "user");
        assert_eq!(turn2_input[0]["content"][0]["text"], "second question");

        assert!(
            reqs[2].get("previous_response_id").is_none(),
            "reset() must drop previous_response_id: {:?}",
            reqs[2]
        );
        // Turn 1 + Turn 2 shared 1 WS connection; Turn 3 opened a fresh one after reset.
        assert_eq!(connections.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn provider_session_falls_back_from_ws_to_http_sse_on_http_only_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ws_attempts = Arc::new(AtomicUsize::new(0));
        let http_attempts = Arc::new(AtomicUsize::new(0));

        let ws_srv = Arc::clone(&ws_attempts);
        let http_srv = Arc::clone(&http_attempts);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let ws_c = Arc::clone(&ws_srv);
                let http_c = Arc::clone(&http_srv);
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    if req.to_ascii_lowercase().contains("upgrade: websocket") {
                        ws_c.fetch_add(1, Ordering::SeqCst);
                        let resp = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                        let _ = socket.write_all(resp.as_bytes()).await;
                        let _ = socket.shutdown().await;
                        return;
                    }
                    http_c.fetch_add(1, Ordering::SeqCst);
                    let sse = concat!(
                        "event: response.output_text.delta\n",
                        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"m1\",\"delta\":\"http-fallback-ok\"}\n\n",
                        "event: response.completed\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"http-fallback-ok\"}]}],\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"total_tokens\":8}}}\n\n",
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        sse.len(),
                        sse
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let mut config = test_model(ApiFormat::OpenAiResponses);
        config.base_url = format!("http://{addr}/v1");
        config.api_key = "sk-test".into();

        // Opt in to WS upgrade over http:// to verify transparent fallback.
        let session = ProviderSession::with_prefer_ws_on_http(true);
        let control = CallControl::new(tokio_util::sync::CancellationToken::new());

        for turn in 0..2 {
            let (tx, mut rx) = tokio::sync::mpsc::channel(32);
            session
                .stream_turn(
                    config.clone(),
                    vec![ChatMsg::user(format!("hello {turn}"))],
                    vec![],
                    tx,
                    control.clone(),
                )
                .await;
            let mut text = String::new();
            let mut failed = false;
            while let Some(ev) = rx.recv().await {
                match ev {
                    ProviderEvent::Text(t) => text.push_str(&t),
                    ProviderEvent::Failed(_) => failed = true,
                    _ => {}
                }
            }
            assert!(!failed, "turn {turn} should fall back transparently without error");
            assert_eq!(text, "http-fallback-ok");
        }
        // WS was attempted only once; turn 2 went straight to HTTP/SSE.
        assert_eq!(ws_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(http_attempts.load(Ordering::SeqCst), 2);
    }
}
