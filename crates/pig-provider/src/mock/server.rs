//! Mock HTTP server: listener, request dispatch, stall handshake and socket writers.

use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::scenarios::*;
use super::streaming::*;
use super::validation::validate_anthropic_request;

pub(super) const MOCK_RESPONSE_HEADERS: &str =
    "x-request-id: mock-request-id\r\nx-trace-id: mock-trace-id\r\ncf-ray: mock-cf-ray\r\n";

/// Body log shared by mock-server helpers.
pub type RequestLog = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Header log (lowercase names, request order) shared by mock-server helpers.
pub type HeaderLog = std::sync::Arc<std::sync::Mutex<Vec<Vec<(String, String)>>>>;

/// Request-path log (request order) shared by mock-server helpers.
pub type PathLog = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Silent time for a normal (unobserved) mock stall.
pub(super) const MOCK_STALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Safety valve for an observed stall whose test panicked before releasing it.
pub(super) const MOCK_STALL_RELEASE_SAFETY: std::time::Duration =
    std::time::Duration::from_secs(60);

/// Sender side of the deterministic stall handshake. The signal marks the
/// handler "active" before announcing it, then blocks until the test releases
/// it; no response bytes or connection close can happen in between.
#[derive(Clone)]
pub struct StallSignal {
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: std::sync::Arc<tokio::sync::Notify>,
    active: std::sync::Arc<AtomicUsize>,
}

impl StallSignal {
    fn enter(&self) {
        self.active.fetch_add(1, Ordering::SeqCst);
        let _ = self.started.send(());
    }

    async fn wait_release(&self) {
        tokio::select! {
            _ = self.release.notified() => {}
            _ = tokio::time::sleep(MOCK_STALL_RELEASE_SAFETY) => {}
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Test-facing handshake: a test waits until the mock is provably holding the
/// response, then asserts that the client returned while `is_stalled()` is
/// still true, and finally releases the handler.
pub struct StallObserver {
    started: tokio::sync::mpsc::UnboundedReceiver<()>,
    release: std::sync::Arc<tokio::sync::Notify>,
    active: std::sync::Arc<AtomicUsize>,
}

impl StallObserver {
    /// Wait for the handler to announce that it has entered the silent phase.
    /// The timeout is only a fail-safe for a missing handshake; it is not the
    /// behavior assertion.
    pub async fn wait_stall_started(&mut self, timeout: std::time::Duration) -> bool {
        matches!(
            tokio::time::timeout(timeout, self.started.recv()).await,
            Ok(Some(()))
        )
    }

    /// Whether the mock is still deliberately holding a response open. The
    /// test checks this after the client returned: true proves the client
    /// ended on its own (idle deadline/cancellation), not because the server
    /// completed or closed.
    pub fn is_stalled(&self) -> bool {
        self.active.load(Ordering::SeqCst) > 0
    }

    pub fn release(&self) {
        self.release.notify_one();
    }
}

/// Start an independent thread running a tokio runtime that serves mock SSE; returns the listening port.
pub fn start_mock_server() -> u16 {
    start_mock_server_with_log().0
}

/// Same as above, but also returns a request body log (for test assertions such as reasoning_params merge).
pub fn start_mock_server_with_log() -> (u16, RequestLog) {
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let port = start_mock_server_inner(Some(log.clone()), None, None, None);
    (port, log)
}

/// Body log plus a per-request header log (lowercase names, request order;
/// for protocol-header assertions such as merged Anthropic beta values).
pub fn start_mock_server_with_headers() -> (u16, RequestLog, HeaderLog) {
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let headers = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<(String, String)>>::new()));
    let port = start_mock_server_inner(Some(log.clone()), Some(headers.clone()), None, None);
    (port, log, headers)
}

/// Body log plus a request-path log (for URL-join assertions shared by live
/// calls, sidecars and the connectivity test).
pub fn start_mock_server_with_paths() -> (u16, RequestLog, PathLog) {
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let port = start_mock_server_inner(Some(log.clone()), None, Some(paths.clone()), None);
    (port, log, paths)
}

/// Body log plus a deterministic stall handshake: the handler announces when
/// it is holding a response open and stays blocked until the test releases it.
/// Tests can therefore prove client-side early return without wall-clock
/// thresholds.
pub fn start_mock_server_with_stall_observer() -> (u16, RequestLog, StallObserver) {
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (started_tx, started_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let active = std::sync::Arc::new(AtomicUsize::new(0));
    let port = start_mock_server_inner(
        Some(log.clone()),
        None,
        None,
        Some(StallSignal {
            started: started_tx,
            release: release.clone(),
            active: active.clone(),
        }),
    );
    (
        port,
        log,
        StallObserver {
            started: started_rx,
            release,
            active,
        },
    )
}

pub(super) fn start_mock_server_inner(
    log: Option<RequestLog>,
    header_log: Option<HeaderLog>,
    path_log: Option<PathLog>,
    stall_signal: Option<StallSignal>,
) -> u16 {
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
            .expect("mock runtime");
        runtime.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
            port_tx
                .send(listener.local_addr().expect("local addr").port())
                .expect("send port");
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                let log = log.clone();
                let header_log = header_log.clone();
                let path_log = path_log.clone();
                let stall_signal = stall_signal.clone();
                tokio::spawn(handle_connection(
                    stream,
                    log,
                    header_log,
                    path_log,
                    stall_signal,
                ));
            }
        });
    });
    port_rx.recv().expect("mock server port")
}

/// Hold the connection silently. With an observer, announce the silent phase
/// and block until the test explicitly releases it, so the test can prove the
/// client returned while the server was still holding the response. Without
/// an observer (ordinary mock use), just sleep for the fixed stall window.
async fn stall_response(stream: &mut tokio::net::TcpStream, signal: Option<&StallSignal>) {
    let Some(signal) = signal else {
        tokio::time::sleep(MOCK_STALL_DEADLINE).await;
        return;
    };
    // The stream must stay alive (and unread) while the client waits.
    let _ = stream;
    signal.enter();
    signal.wait_release().await;
}

/// `EARLY_TOOL_TRIGGER <url>` (chat format): the first call, a `FetchURL` of
/// `<url>`, arrives complete, then the response holds until the test releases
/// it, and only then do a second call and the finish frame arrive. The hold
/// separates "this call's arguments are complete" from "the stream has
/// ended"; the fetch is what makes an early start observable.
async fn write_early_tool_scenario(
    stream: &mut tokio::net::TcpStream,
    body: &str,
    stall_signal: Option<&StallSignal>,
) {
    let url = outside_path_after(body, EARLY_TOOL_TRIGGER).unwrap_or_default();
    let first = sse_chunk(
        serde_json::json!({"tool_calls": [{
            "index": 0,
            "id": "call_early_first",
            "function": {
                "name": "FetchURL",
                "arguments": serde_json::json!({"url": url}).to_string(),
            },
        }]}),
        None,
    );
    let second = sse_chunk(
        serde_json::json!({"tool_calls": [{
            "index": 1,
            "id": "call_early_second",
            "function": {
                "name": "Read",
                "arguments": serde_json::json!({"path": MOCK_FILE_NAME}).to_string(),
            },
        }]}),
        None,
    );
    let _ = stream.write_all(first.as_bytes()).await;
    let _ = stream.flush().await;
    stall_response(stream, stall_signal).await;
    let _ = stream.write_all(second.as_bytes()).await;
    let finish = format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "mock-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 60, "completion_tokens": 10, "total_tokens": 70},
        })
    );
    let _ = stream.write_all(finish.as_bytes()).await;
}

/// The steer scenarios (chat format). `tool_input` picks the first frame: a
/// tool call whose arguments are still open, instead of a text delta. The
/// frames after the hold complete whatever the first one started.
async fn write_steer_stall_scenario(
    stream: &mut tokio::net::TcpStream,
    tool_input: bool,
    stall_signal: Option<&StallSignal>,
) {
    let finish = |reason: &str| {
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
                "usage": {"prompt_tokens": 60, "completion_tokens": 10, "total_tokens": 70},
            })
        )
    };
    let (first, rest, reason) = if tool_input {
        (
            sse_chunk(
                serde_json::json!({"tool_calls": [{
                    "index": 0,
                    "id": "call_steer_hold",
                    "function": {"name": "Read", "arguments": "{\"path\": \"REA"},
                }]}),
                None,
            ),
            sse_chunk(
                serde_json::json!({"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "DME.mock.md\"}"},
                }]}),
                None,
            ),
            "tool_calls",
        )
    } else {
        (
            sse_chunk(serde_json::json!({"content": STEER_STALL_TEXT}), None),
            sse_chunk(serde_json::json!({"content": STEER_STALL_TAIL}), None),
            "stop",
        )
    };
    let _ = stream.write_all(first.as_bytes()).await;
    let _ = stream.flush().await;
    stall_response(stream, stall_signal).await;
    let _ = stream.write_all(rest.as_bytes()).await;
    let _ = stream.write_all(finish(reason).as_bytes()).await;
}

/// Thinking ticker demo scenario (TICKER_SCENARIO): does not use the unified
/// 50ms write loop — thinking chunks are written with the script's own delays,
/// and the body reply returns to the normal rhythm.
async fn write_ticker_scenario(stream: &mut tokio::net::TcpStream) {
    for (ms, piece) in ticker_scenario_script() {
        if ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
        // Line-spacing placeholder chunks produce no content (pure delay)
        if piece.is_empty() {
            continue;
        }
        let chunk = sse_chunk(serde_json::json!({"reasoning_content": piece}), None);
        if stream.write_all(chunk.as_bytes()).await.is_err() {
            return;
        }
    }
    let reply = format!("Ticker demo finished: {TICKER_MARKER}.");
    let chars: Vec<char> = reply.chars().collect();
    for piece in chars.chunks(6) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let delta: String = piece.iter().collect();
        let chunk = sse_chunk(serde_json::json!({"content": delta}), None);
        if stream.write_all(chunk.as_bytes()).await.is_err() {
            return;
        }
    }
    let stop = sse_chunk(serde_json::json!({}), Some("stop"));
    let _ = stream.write_all(stop.as_bytes()).await;
    let _ = stream.write_all(b"data: [DONE]\n\n").await;
    let _ = stream.shutdown().await;
}

/// The stream-all scenario carries per-fragment delays (same bypass as the
/// ticker demo — the unified 50ms write loop would outrun the 1s preview
/// throttle and collapse the growth into first-line → final jump)
async fn write_stream_all_scenario(
    stream: &mut tokio::net::TcpStream,
    anthropic: bool,
    responses: bool,
    tool_results: usize,
) {
    let frames = if responses {
        stream_all_responses_frames(tool_results)
    } else if anthropic {
        stream_all_anthropic_frames(tool_results)
    } else {
        stream_all_chat_frames(tool_results)
    };
    for (ms, frame) in frames {
        if ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
        if stream.write_all(frame.as_bytes()).await.is_err() {
            return;
        }
    }
    // Anthropic has no [DONE] terminator (same as the unified write loop)
    if !anthropic {
        let _ = stream.write_all(b"data: [DONE]\n\n").await;
    }
    let _ = stream.shutdown().await;
}

/// Non-streaming auto-naming response: {"title": MOCK_TITLE} (same content for
/// both API formats). Request recognition is in session::TITLE_PROMPT_MARKER
async fn write_title_response(
    stream: &mut tokio::net::TcpStream,
    anthropic: bool,
    responses: bool,
) -> bool {
    let content = format!("{{\"title\":\"{MOCK_TITLE}\"}}");
    let json = if responses {
        serde_json::json!({
            "id": "resp-mock",
            "output": [{"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": content}]}],
            "usage": {"input_tokens": 50, "output_tokens": 10, "total_tokens": 60},
        })
    } else if anthropic {
        serde_json::json!({
            "id": "msg-mock",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": content}],
            "model": "mock-model",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 50, "output_tokens": 10},
        })
    } else {
        serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "created": 0,
            "model": "mock-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 50, "completion_tokens": 10, "total_tokens": 60},
        })
    };
    let payload = json.to_string();
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{MOCK_RESPONSE_HEADERS}\r\n{}",
        payload.len(),
        payload
    );
    stream.write_all(resp.as_bytes()).await.is_ok()
}

/// Non-streaming response (compact summary). FAIL_COMPACT triggers a 500 to test the fallback path.
async fn write_json_response(stream: &mut tokio::net::TcpStream, body: &str) -> bool {
    if body.contains(SIDECAR_EMPTY_TOOL_CALLS_TRIGGER) {
        let json = serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "created": 0,
            "model": "mock-model",
            "choices": [{"index": 0, "message": {
                "role": "assistant",
                "content": SIDECAR_EMPTY_TOOL_CALLS_TEXT,
                "tool_calls": [],
            }, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13},
        });
        let payload = json.to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{MOCK_RESPONSE_HEADERS}\r\n{}",
            payload.len(),
            payload
        );
        return stream.write_all(resp.as_bytes()).await.is_ok();
    }
    if body.contains(SIDECAR_CONTENT_ARRAY_TRIGGER) {
        let json = serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "created": 0,
            "model": "mock-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": [
                {"type": "text", "text": "array "},
                {"type": "output_text", "text": "joined"},
            ]}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13},
        });
        let payload = json.to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{MOCK_RESPONSE_HEADERS}\r\n{}",
            payload.len(),
            payload
        );
        return stream.write_all(resp.as_bytes()).await.is_ok();
    }
    if body.contains("FAIL_COMPACT") {
        let resp = format!(
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n{MOCK_RESPONSE_HEADERS}\r\n{{}}"
        );
        return stream.write_all(resp.as_bytes()).await.is_ok();
    }
    let content =
        format!("{SUMMARY_MARKER}: goal=mock selftest; done=read/write files; todo=none.");
    let json = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": "mock-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 500, "completion_tokens": 30, "total_tokens": 530},
    });
    let payload = json.to_string();
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{MOCK_RESPONSE_HEADERS}\r\n{}",
        payload.len(),
        payload
    );
    stream.write_all(resp.as_bytes()).await.is_ok()
}

/// Consume one unit of a one-shot scenario counter.
///
/// Returns `false` once the counter reaches zero (the scenario is exhausted).
/// A CAS loop rather than `AtomicUsize::fetch_update`: the latter is deprecated
/// on newer toolchains in favour of `try_update`, which older toolchains do not
/// have, and this mock is built by every downstream test target.
pub(super) fn consume_one_shot(counter: &AtomicUsize) -> bool {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current == 0 {
            return false;
        }
        match counter.compare_exchange_weak(
            current,
            current - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    log: Option<RequestLog>,
    header_log: Option<HeaderLog>,
    path_log: Option<PathLog>,
    stall_signal: Option<StallSignal>,
) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte).await.unwrap_or(0) == 0 {
            return;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > 64 * 1024 {
            return;
        }
    }
    let head_text = String::from_utf8_lossy(&head);
    if let Some(header_log) = &header_log {
        let headers = head_text
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect::<Vec<_>>();
        header_log.lock().expect("header log lock").push(headers);
    }
    let path = head_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    if let Some(path_log) = &path_log {
        path_log.lock().expect("path log lock").push(path.clone());
    }
    let content_length: usize = head_text
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 && stream.read_exact(&mut body).await.is_err() {
        return;
    }
    let body = String::from_utf8_lossy(&body).to_string();
    if let Some(log) = &log {
        log.lock().expect("log lock").push(body.clone());
    }

    // Retry test: the first request containing FAIL_ONCE_500 returns 500 (normal once the count is exhausted)
    pub(super) static FAIL_ONCE_500: AtomicUsize = AtomicUsize::new(1);
    // Empty-completion test: the first STREAMING request containing
    // EMPTY_ONCE gets a content-less stream (normal once exhausted)
    static EMPTY_ONCE: AtomicUsize = AtomicUsize::new(1);
    if body.contains("FAIL_ONCE_500") && consume_one_shot(&FAIL_ONCE_500) {
        let resp = format!(
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n{MOCK_RESPONSE_HEADERS}\r\n{{}}"
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
        return;
    }
    static RETRY_429_ONCE: AtomicUsize = AtomicUsize::new(1);
    if body.contains(RETRY_429_ONCE_TRIGGER) && consume_one_shot(&RETRY_429_ONCE) {
        let resp = format!(
            "HTTP/1.1 429 Too Many Requests\r\nretry-after: 0\r\ncontent-length: 2\r\n{MOCK_RESPONSE_HEADERS}\r\n{{}}"
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
        return;
    }
    // Streaming error status whose body stalls after a partial write: the
    // client's error-body read must hit its idle deadline (or cancellation)
    // instead of waiting for this handler to close.
    if body.contains(ERROR_BODY_STALL_TRIGGER) {
        let head = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n{MOCK_RESPONSE_HEADERS}\r\n"
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(b"{\"error\":").await;
        stall_response(&mut stream, stall_signal.as_ref()).await;
        return;
    }
    // A complete context-overflow JSON object followed by an unfinished body:
    // the idle failure must be classified from the bytes already received.
    if body.contains(CONTEXT_OVERFLOW_STALL_TRIGGER) {
        let payload = serde_json::json!({
            "error": {
                "message": CONTEXT_OVERFLOW_MARKER,
                "type": "invalid_request_error",
                "code": "context_length_exceeded",
            },
        })
        .to_string();
        let head = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{MOCK_RESPONSE_HEADERS}\r\n",
            payload.len() + 64
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(payload.as_bytes()).await;
        stall_response(&mut stream, stall_signal.as_ref()).await;
        return;
    }
    // A connectivity probe's error body stalls mid-transfer.
    if body.contains(CONN_TEST_ERROR_BODY_STALL_TRIGGER) {
        let head = format!(
            "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n{MOCK_RESPONSE_HEADERS}\r\n"
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream
            .write_all(b"{\"error\":{\"message\":\"stalled\"}")
            .await;
        stall_response(&mut stream, stall_signal.as_ref()).await;
        return;
    }

    let anthropic = path.ends_with("/messages");
    let responses = path.ends_with("/responses");
    // The mock refuses invalid Anthropic shapes instead of certifying them:
    // a request our encoder produces must be valid, not merely accepted.
    if anthropic
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&body)
        && let Err(problem) = validate_anthropic_request(&value)
    {
        let payload = serde_json::json!({
            "error": {
                "type": "invalid_request_error",
                "message": format!("mock rejected invalid Anthropic request shape: {problem}"),
            }
        })
        .to_string();
        let resp = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{MOCK_RESPONSE_HEADERS}\r\n{}",
            payload.len(),
            payload
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
        return;
    }
    let tool_results = body.matches("\"role\":\"tool\"").count()
        + body.matches("\"role\": \"tool\"").count()
        + body.matches("tool_result").count()
        + body.matches("function_call_output").count();
    let user_text = last_user_text(&body);

    if body.contains(CONTEXT_OVERFLOW_KIMI_TRIGGER) {
        let payload = serde_json::json!({
            "error": {"type": "invalid_request_error", "message": CONTEXT_OVERFLOW_KIMI_MARKER},
        })
        .to_string();
        let resp = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{MOCK_RESPONSE_HEADERS}\r\n{}",
            payload.len(),
            payload
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
        return;
    }
    if body.contains(CONTEXT_OVERFLOW_GLM_TRIGGER) {
        let payload = serde_json::json!({
            "error": {"code": "1261", "message": CONTEXT_OVERFLOW_GLM_MARKER},
        })
        .to_string();
        let resp = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{MOCK_RESPONSE_HEADERS}\r\n{}",
            payload.len(),
            payload
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
        return;
    }
    if body.contains(CONTEXT_OVERFLOW_HTTP_TRIGGER) {
        let payload = serde_json::json!({
            "error": {
                "message": CONTEXT_OVERFLOW_MARKER,
                "type": "invalid_request_error",
                "code": "context_length_exceeded",
            },
        })
        .to_string();
        let resp = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{MOCK_RESPONSE_HEADERS}\r\n{}",
            payload.len(),
            payload
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
        return;
    }

    // Non-streaming = auto naming / compact summary / connectivity test (must branch before the SSE response head)
    if body.contains("\"stream\":false") {
        if body.contains(SIDECAR_STALL_HEADERS_TRIGGER) {
            stall_response(&mut stream, stall_signal.as_ref()).await;
            return;
        }
        if body.contains(SIDECAR_STALL_BODY_TRIGGER) {
            // `connection: close` makes the client drop this connection rather
            // than return a half-read body to the pool.
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: 1000\r\n{MOCK_RESPONSE_HEADERS}\r\n"
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(b"{\"choices\":[").await;
            stall_response(&mut stream, stall_signal.as_ref()).await;
            return;
        }
        let ok = if body.contains(pig_protocol::TITLE_PROMPT_MARKER) {
            write_title_response(&mut stream, anthropic, responses).await
        } else if responses {
            write_json_response_responses(&mut stream, &body).await
        } else if anthropic {
            write_json_response_anthropic(&mut stream, &body).await
        } else {
            write_json_response(&mut stream, &body).await
        };
        if !ok {
            return;
        }
        let _ = stream.shutdown().await;
        return;
    }

    let response_head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n{MOCK_RESPONSE_HEADERS}\r\n"
    );
    if stream.write_all(response_head.as_bytes()).await.is_err() {
        return;
    }
    // Idle-timeout scenario: one valid frame, then silence without closing.
    if body.contains(STALL_STREAM_TRIGGER) {
        let frame = if anthropic {
            a_sse(
                "content_block_delta",
                serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": STALL_STREAM_TEXT}}),
            )
        } else if responses {
            r_ev(serde_json::json!({
                "type": "response.output_text.delta", "output_index": 0, "delta": STALL_STREAM_TEXT,
            }))
        } else {
            sse_chunk(serde_json::json!({"content": STALL_STREAM_TEXT}), None)
        };
        let _ = stream.write_all(frame.as_bytes()).await;
        stall_response(&mut stream, stall_signal.as_ref()).await;
        return;
    }
    // Early-execution scenario: the first call is complete and the stream is
    // held before anything else arrives. Only the turn whose last user
    // message carries the trigger plays it.
    if tool_results == 0 && user_text.contains(EARLY_TOOL_TRIGGER) {
        write_early_tool_scenario(&mut stream, &body, stall_signal.as_ref()).await;
        return;
    }
    // Steer scenarios: the stream is held after one frame, so a test can send
    // a steer while the model's answer is demonstrably still in flight.
    if tool_results == 0 && user_text.contains(STEER_TOOL_STALL_TRIGGER) {
        write_steer_stall_scenario(&mut stream, true, stall_signal.as_ref()).await;
        return;
    }
    if tool_results == 0 && user_text.contains(STEER_STALL_TRIGGER) {
        write_steer_stall_scenario(&mut stream, false, stall_signal.as_ref()).await;
        return;
    }
    // Thinking ticker demo scenario: variable-speed emission (each chunk
    // carries its own delay), not the unified 50ms write loop
    if !anthropic && body.contains(SCENARIO_TICKER_TRIGGER) {
        write_ticker_scenario(&mut stream).await;
        return;
    }
    // Streaming-all-tools demo scenario: per-fragment delays (same bypass —
    // the unified 50ms loop would outrun the 1s preview throttle)
    if body.contains(STREAM_ALL_TRIGGER) {
        write_stream_all_scenario(&mut stream, anthropic, responses, tool_results).await;
        return;
    }
    let chunks = if responses {
        responses_chunks(&body, tool_results)
    } else if anthropic {
        anthropic_chunks(&body, tool_results)
    } else if body.contains(STRUCTURED_OUTPUT_TRIGGER) {
        vec![
            sse_chunk(serde_json::json!({"content": "{\"answer\":\"ok\"}"}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ]
    } else if body.contains(UNKNOWN_FINISH_TRIGGER) {
        vec![
            sse_chunk(serde_json::json!({"content": UNKNOWN_TERMINAL_TEXT}), None),
            sse_chunk(
                serde_json::json!({"tool_calls": [{
                    "index": 0,
                    "id": "call_unknown_finish",
                    "function": {"name": "Write", "arguments": "{\"path\":\"unknown-must-not-write.txt\",\"content\":\"x\"}"},
                }]}),
                Some("banana"),
            ),
        ]
    } else if body.contains(EMPTY_USAGE_ONCE_TRIGGER) {
        static EMPTY_USAGE_ONCE: AtomicUsize = AtomicUsize::new(1);
        if consume_one_shot(&EMPTY_USAGE_ONCE) {
            vec![format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chatcmpl-mock",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 9, "completion_tokens": 0, "total_tokens": 9},
                })
            )]
        } else {
            vec![
                sse_chunk(
                    serde_json::json!({"content": EMPTY_USAGE_ONCE_MARKER}),
                    None,
                ),
                format!(
                    "data: {}\n\n",
                    serde_json::json!({
                        "id": "chatcmpl-mock",
                        "object": "chat.completion.chunk",
                        "created": 0,
                        "model": "mock-model",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17},
                    })
                ),
            ]
        }
    } else if body.contains(MISSING_FINISH_TRIGGER) {
        vec![sse_chunk(
            serde_json::json!({"content": "unterminated answer"}),
            None,
        )]
    } else if body.contains(MALFORMED_STREAM_TRIGGER) {
        vec!["data: {not-json}\n\n".to_string()]
    } else if body.contains(FINISH_LENGTH_TRIGGER) {
        openai_incomplete("length")
    } else if body.contains(FINISH_CONTENT_FILTER_TRIGGER) {
        openai_incomplete("content_filter")
    } else if body.contains(CONTEXT_OVERFLOW_KIMI_STREAM_TRIGGER) {
        vec![format!(
            "data: {}\n\n",
            serde_json::json!({"error": {"type": "invalid_request_error", "message": CONTEXT_OVERFLOW_KIMI_MARKER}})
        )]
    } else if body.contains(CONTEXT_OVERFLOW_GLM_STREAM_TRIGGER) {
        vec![format!(
            "data: {}\n\n",
            serde_json::json!({"success": false, "code": 1261, "message": CONTEXT_OVERFLOW_GLM_MARKER})
        )]
    } else if body.contains(CONTEXT_OVERFLOW_STREAM_TRIGGER) {
        vec![format!(
            "data: {}\n\n",
            serde_json::json!({"error": {"message": CONTEXT_OVERFLOW_MARKER, "code": "context_length_exceeded"}})
        )]
    } else if body.contains(BUSINESS_ERROR_TRIGGER) {
        // Business error inside the 200 stream (OpenAI-compatible gateways)
        vec![format!(
            "data: {}\n\n",
            serde_json::json!({"error": {"message": BUSINESS_ERROR_MARKER, "code": "1005"}})
        )]
    } else if body.contains(USAGE_TAIL_TRIGGER) {
        vec![
            sse_chunk(serde_json::json!({"content": "usage tail"}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
            format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chatcmpl-mock",
                    "choices": [],
                    "usage": {"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17},
                })
            ),
        ]
    } else if body.contains(EMPTY_ONCE_TRIGGER) && consume_one_shot(&EMPTY_ONCE) {
        // Empty completion: a 200 stream whose only chunk is a bare stop
        vec![sse_chunk(serde_json::json!({}), Some("stop"))]
    } else if body.contains(EMPTY_ONCE_TRIGGER) {
        vec![
            sse_chunk(serde_json::json!({"content": EMPTY_ONCE_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ]
    } else if body.contains(MISSING_USAGE_AFTER_FIRST_TRIGGER) {
        missing_usage_after_first_response(&body)
    } else if body.contains("ECHO_USAGE") {
        echo_usage_response(&body)
    } else if body.contains("ECHO_HISTORY") {
        echo_history_response(&body)
    } else if body.contains(SCENARIO_C_TRIGGER) {
        scenario_c_response(tool_results)
    } else if body.contains(TODO_SCENARIO_TRIGGER) {
        todo_scenario_response(&body)
    } else if body.contains(MEMORY_WRITE_TRIGGER) {
        memory_write_response(&body, tool_results)
    } else if body.contains(SCENARIO_Q_TRIGGER) {
        question_scenario_response(&body)
    } else if body.contains(DUPLICATE_SERIAL_ID_TRIGGER) {
        duplicate_tool_id_response(tool_results, true)
    } else if body.contains(DUPLICATE_PARALLEL_ID_TRIGGER) {
        duplicate_tool_id_response(tool_results, false)
    } else if body.contains(SCENARIO_DANGER_TRIGGER) {
        danger_scenario_response(tool_results)
    } else if body.contains(SCENARIO_SUBJECT_TRIGGER) {
        subject_scenario_response(tool_results)
    } else if body.contains(SCENARIO_READONLY_TRIGGER) {
        readonly_scenario_response(tool_results)
    } else if user_text.contains(SCENARIO_BACKGROUND_FAILURE_TRIGGER) {
        background_failure_scenario_response(&body)
    } else if user_text.contains(SCENARIO_BACKGROUND_TRIGGER) {
        background_task_scenario_response(&body)
    } else if user_text.contains(SCENARIO_TASK_WAIT_TRIGGER) {
        task_wait_scenario_response(&body)
    } else if body.contains(SCENARIO_SLOW_TRIGGER) {
        slow_bash_scenario_response(tool_results)
    } else if body.contains(SCENARIO_SLOW_FOLLOWUP_TRIGGER) {
        slow_followup_response()
    } else if body.contains(SCENARIO_PLAN_EXIT_TRIGGER) {
        plan_exit_scenario_response(&body, tool_results)
    } else if body.contains(SCENARIO_PLAN_ENTER_TRIGGER) {
        plan_enter_scenario_response(tool_results)
    } else if body.contains(SCENARIO_PLAN_FILE_TRIGGER) {
        plan_file_scenario_response(&body, tool_results)
    } else if body.contains(SCENARIO_PLAN_WRITE_GATE_TRIGGER) {
        plan_write_gate_response(tool_results)
    } else if let Some(root) = outside_path_after(&body, SESSION_GRANT_TRIGGER) {
        session_grant_response(tool_results, &root)
    } else if body.contains(PROJECT_RULE_TRIGGER) {
        project_rule_response(tool_results)
    } else if let Some(path) = outside_path_after(&body, CREDENTIAL_AUTO_READ_TRIGGER) {
        credential_auto_read_response(tool_results, &path)
    } else if let Some(path) = outside_path_after(&body, CREDENTIAL_READ_TRIGGER) {
        credential_read_response(tool_results, &path)
    } else if let Some(path) = outside_path_after(&body, CREDENTIAL_LISTING_TRIGGER) {
        credential_listing_response(tool_results, &path)
    } else if let Some(path) = outside_path_after(&body, DENIED_WIDENING_TRIGGER) {
        denied_widening_response(tool_results, &path)
    } else if let Some(path) = outside_path_after(&body, CREDENTIAL_WRITE_TRIGGER) {
        credential_write_response(tool_results, &path)
    } else if let Some(path) = outside_path_after(&body, CREDENTIAL_DIR_TRIGGER) {
        credential_dir_response(tool_results, &path)
    } else if let Some(path) = outside_path_after(&body, CREDENTIAL_FILE_TRIGGER) {
        credential_file_response(tool_results, &path)
    } else if let Some(path) = outside_path_after(&body, SCENARIO_REQUIRE_ESCALATED_TRIGGER) {
        require_escalated_response(tool_results, &path)
    } else if let Some(path) = outside_path_after(&body, SCENARIO_ESCALATION_TRIGGER) {
        escalation_response(tool_results, &path)
    } else if body.contains(SCENARIO_PLAN_BASH_TRIGGER) {
        plan_bash_response(tool_results, plan_bash_command())
    } else if body.contains(SCENARIO_MEDIA_TRIGGER) {
        media_scenario_response(tool_results)
    } else if body.contains(PREFIX_PARALLEL_SCENARIO) {
        prefix_parallel_response(tool_results)
    } else if let Some(path) = outside_path_after(&body, SCENARIO_OUTSIDE_READ_TRIGGER).as_deref() {
        // Multi-turn tests reuse the trigger: count tool results after the
        // LAST user message, not across the whole history
        let tail = body.rsplit("\"role\":\"user\"").next().unwrap_or("");
        outside_read_response(path, tail.matches("\"role\":\"tool\"").count())
    } else if let Some(path) = outside_path_after(&body, SCENARIO_OUTSIDE_READ2_TRIGGER).as_deref()
    {
        let tail = body.rsplit("\"role\":\"user\"").next().unwrap_or("");
        outside_read2_response(path, tail.matches("\"role\":\"tool\"").count())
    } else if let Some(path) = outside_path_after(&body, SCENARIO_OUTSIDE_WRITE_TRIGGER).as_deref()
    {
        let tail = body.rsplit("\"role\":\"user\"").next().unwrap_or("");
        outside_write_response(path, tail.matches("\"role\":\"tool\"").count())
    } else if body.contains(SUBAGENT_TRIGGER) {
        // Parent-side requests always contain the original trigger; child-side
        // requests only have the behavior token prefix from the prompt
        subagent_parent_response(&body)
    } else if body.contains(SUBAGENT_CHILD_PREFIX) {
        subagent_child_response(&body, tool_results)
    } else if body.contains(STREAM_ARGS_TRIGGER) {
        stream_args_response(tool_results)
    } else if body.contains(SCENARIO_B_TRIGGER) {
        scenario_b_response(tool_results, SCENARIO_B_FILE)
    } else if user_text.contains(REPEAT_LOOP_PARALLEL_TRIGGER) {
        repeat_loop_parallel_response(tool_results)
    } else if user_text.contains(REPEAT_LOOP_TRIGGER) {
        repeat_loop_response(tool_results)
    } else if tool_results > 0 {
        text_response()
    } else {
        tool_call_response()
    };
    for chunk in chunks {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if stream.write_all(chunk.as_bytes()).await.is_err() {
            return;
        }
    }
    if !anthropic {
        let _ = stream.write_all(b"data: [DONE]\n\n").await;
    }
    let _ = stream.shutdown().await;
}

/// Non-streaming Responses reply (compact summary): one message item
async fn write_json_response_responses(stream: &mut tokio::net::TcpStream, body: &str) -> bool {
    if body.contains("FAIL_COMPACT") {
        let resp = format!(
            "HTTP/1.1 500 Internal Server Error
content-length: 2
{MOCK_RESPONSE_HEADERS}
{{}}"
        );
        return stream.write_all(resp.as_bytes()).await.is_ok();
    }
    let content =
        format!("{SUMMARY_MARKER}: goal=mock selftest; done=read/write files; todo=none.");
    let json = serde_json::json!({
        "id": "resp-mock",
        "output": [{"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": content}]}],
        "usage": {"input_tokens": 500, "output_tokens": 30, "total_tokens": 530},
    });
    let payload = json.to_string();
    let resp = format!(
        "HTTP/1.1 200 OK
content-type: application/json
content-length: {}
{MOCK_RESPONSE_HEADERS}
{}",
        payload.len(),
        payload
    );
    stream.write_all(resp.as_bytes()).await.is_ok()
}

async fn write_json_response_anthropic(stream: &mut tokio::net::TcpStream, body: &str) -> bool {
    if body.contains("FAIL_COMPACT") {
        let resp = format!(
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n{MOCK_RESPONSE_HEADERS}\r\n{{}}"
        );
        return stream.write_all(resp.as_bytes()).await.is_ok();
    }
    let content =
        format!("{SUMMARY_MARKER}: goal=mock selftest; done=read/write files; todo=none.");
    let json = serde_json::json!({
        "id": "msg-mock",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": content}],
        "model": "mock-model",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 500, "output_tokens": 30},
    });
    let payload = json.to_string();
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{MOCK_RESPONSE_HEADERS}\r\n{}",
        payload.len(),
        payload
    );
    stream.write_all(resp.as_bytes()).await.is_ok()
}
