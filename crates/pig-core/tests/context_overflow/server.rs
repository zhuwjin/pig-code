use std::io::{Read as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pig_protocol::{ApiFormat, Event};
use pig_provider::mock;

use crate::common;
use crate::fixtures::{config, workspace};

#[derive(Clone, Copy, Debug)]
pub(crate) enum ServerMode {
    /// Streaming: one text delta, then an in-band context-window error.
    ForwardedOverflow,
    /// Summary requests containing the marker get a 400; other summary
    /// requests succeed once the marker segment is dropped.
    SummaryShrink,
    /// Summary requests containing the marker always get a 400.
    SummaryFallback,
    /// Automatic preflight: the first trigger step reports a large usage, then
    /// every summary request overflows; automatic compaction must take the
    /// bounded note-only fallback and continue the turn.
    AutoSummaryFallback,
    /// Streaming tool-call step reporting a large realistic usage total, then a
    /// normal text answer. Non-streaming requests return a summary.
    Preflight,
    /// Summary requests block long enough for the test to interrupt them.
    SlowSummary,
    /// One single-user-segment preflight: a tool call reporting a large usage,
    /// then text once the compact note is present (prevents a compaction loop).
    SingleSegmentPreflight,
    /// One single-assistant-round overflow: the first streaming request whose
    /// tool result carries the trigger gets a typed overflow; after the retry
    /// (trigger compacted away) the tool call and the next text are normal.
    SingleRoundOverflow,
    /// The same overflow boundary after a real Write side effect.
    SideEffectOverflow,
    /// A typed overflow whose only omitted work is smaller than the generated
    /// summary: the compaction estimate grows, so the existing guard must
    /// surface the typed error instead of retrying.
    OverflowGrowth,
    /// Streaming requests containing the marker report a large usage; other
    /// requests stay small. Used by the recall-watermark regression (the
    /// recalled turn's high sample must not drive the next preflight).
    HighUsage,
}

pub(crate) struct TestServer {
    port: u16,
    pub(crate) bodies: Arc<Mutex<Vec<String>>>,
}

fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(head_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            let body_start = head_end + 4;
            while buffer.len() < body_start + length {
                let read = stream.read(&mut chunk).unwrap_or(0);
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            return String::from_utf8_lossy(&buffer[body_start..]).to_string();
        }
    }
    String::new()
}

fn sse_response(chunks: &[String]) -> String {
    let body: String = chunks.iter().map(|chunk| format!("{chunk}\n\n")).collect();
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn json_response(status: &str, payload: &serde_json::Value) -> String {
    let body = payload.to_string();
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn text_stream(text: &str, total: u64) -> String {
    sse_response(&[
        format!(
            "data: {}",
            serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {"content": text}}],
            })
        ),
        format!(
            "data: {}",
            serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": total},
            })
        ),
        "data: [DONE]".to_string(),
    ])
}

fn tool_call_stream(total: u64) -> String {
    tool_call_stream_for("Read", serde_json::json!({"path": "README.mock.md"}), total)
}

fn tool_call_stream_for(tool: &str, arguments: serde_json::Value, total: u64) -> String {
    sse_response(&[
        format!(
            "data: {}",
            serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_srv_1",
                    "type": "function",
                    "function": {
                        "name": tool,
                        "arguments": arguments.to_string(),
                    },
                }]}}],
            })
        ),
        format!(
            "data: {}",
            serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 12_000, "completion_tokens": 10, "total_tokens": total},
            })
        ),
        "data: [DONE]".to_string(),
    ])
}

fn summary_response() -> String {
    json_response(
        "200 OK",
        &serde_json::json!({
            "id": "chatcmpl-summary",
            "object": "chat.completion",
            "created": 0,
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "SERVER_SUMMARY"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        }),
    )
}

/// A summary far larger than the tiny omitted tool round, used to prove that
/// a growing post-compaction estimate is not retried.
fn long_summary_response() -> String {
    json_response(
        "200 OK",
        &serde_json::json!({
            "id": "chatcmpl-summary",
            "object": "chat.completion",
            "created": 0,
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "GROWTH_SUMMARY ".repeat(400)},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        }),
    )
}

fn overflow_response() -> String {
    json_response(
        "400 Bad Request",
        &serde_json::json!({
            "error": {
                "message": "This model's maximum context length is 8192 tokens",
                "type": "invalid_request_error",
                "code": "context_length_exceeded",
            },
        }),
    )
}

pub(crate) fn start_server(mode: ServerMode) -> TestServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let bodies_thread = bodies.clone();
    let overflow_seen = Arc::new(AtomicBool::new(false));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = read_http_request(&mut stream);
            bodies_thread.lock().unwrap().push(body.clone());
            let streaming = body.contains("\"stream\":true");
            let response = match mode {
                ServerMode::ForwardedOverflow => {
                    let chunks = [
                        format!(
                            "data: {}",
                            serde_json::json!({"choices": [{"index": 0, "delta": {"content": "partial"}}]})
                        ),
                        format!(
                            "data: {}",
                            serde_json::json!({"error": {"message": "maximum context length is 8192 tokens", "code": "context_length_exceeded"}})
                        ),
                    ];
                    sse_response(&chunks)
                }
                ServerMode::SummaryShrink => {
                    if !streaming && body.contains("SUMMARY_DROP_MARKER") {
                        overflow_response()
                    } else if streaming {
                        text_stream("SERVER_STREAM_DONE", 100)
                    } else {
                        summary_response()
                    }
                }
                ServerMode::SummaryFallback => {
                    if !streaming && body.contains("SUMMARY_KEEP_MARKER") {
                        overflow_response()
                    } else if streaming {
                        text_stream("SERVER_STREAM_DONE", 100)
                    } else {
                        summary_response()
                    }
                }
                ServerMode::Preflight => {
                    if streaming {
                        let has_tool_result = body.contains("\"role\":\"tool\"");
                        if body.contains("PREFLIGHT_SCENARIO") && !has_tool_result {
                            tool_call_stream(16_000)
                        } else {
                            text_stream("SERVER_PREFLIGHT_DONE", 100)
                        }
                    } else {
                        summary_response()
                    }
                }
                ServerMode::HighUsage => {
                    if !streaming {
                        summary_response()
                    } else if body.contains("HIGH_USAGE_SCENARIO") {
                        text_stream("SERVER_HIGH_USAGE_DONE", 120_000)
                    } else {
                        text_stream("SERVER_RECALL_DONE", 100)
                    }
                }
                ServerMode::AutoSummaryFallback => {
                    if !streaming {
                        overflow_response()
                    } else if body.contains("AUTO_FALLBACK_SCENARIO")
                        && !body.contains("\"role\":\"tool\"")
                        && !body.contains("Earlier context compacted")
                    {
                        tool_call_stream(120_000)
                    } else {
                        text_stream("SERVER_AUTO_FALLBACK_DONE", 100)
                    }
                }
                ServerMode::SlowSummary => {
                    if !streaming && body.contains("[COMPACTION]") {
                        std::thread::sleep(Duration::from_secs(5));
                        summary_response()
                    } else if streaming {
                        text_stream("SERVER_SLOW_DONE", 100)
                    } else {
                        summary_response()
                    }
                }
                ServerMode::SingleSegmentPreflight => {
                    if streaming {
                        let has_tool_result = body.contains("\"role\":\"tool\"");
                        if body.contains("PREFLIGHT_SINGLE_SCENARIO")
                            && !has_tool_result
                            && !body.contains("Earlier context compacted")
                        {
                            tool_call_stream(16_000)
                        } else {
                            text_stream("SERVER_PREFLIGHT_SINGLE_DONE", 100)
                        }
                    } else {
                        summary_response()
                    }
                }
                ServerMode::SingleRoundOverflow | ServerMode::SideEffectOverflow => {
                    let awaiting_tool = if matches!(mode, ServerMode::SideEffectOverflow) {
                        !body.contains("call_srv_1")
                    } else {
                        !body.contains("\"role\":\"tool\"")
                    };
                    if !streaming {
                        summary_response()
                    } else if body.contains(mock::CONTEXT_OVERFLOW_STREAM_TRIGGER)
                        && body.contains("\"role\":\"tool\"")
                        && body.contains("call_srv_1")
                    {
                        if overflow_seen.swap(true, Ordering::SeqCst) {
                            text_stream("SERVER_OVERFLOW_RECOVERED", 100)
                        } else {
                            overflow_response()
                        }
                    } else if awaiting_tool && !body.contains("Earlier context compacted") {
                        if matches!(mode, ServerMode::SideEffectOverflow) {
                            let command = if cfg!(windows) {
                                "[System.IO.File]::WriteAllText('overflow-side-effect.txt', 'written once'); for ($i = 0; $i -lt 2000; $i++) { [Console]::WriteLine('payload line') }"
                            } else {
                                "printf 'written once' > overflow-side-effect.txt; for i in $(seq 1 2000); do echo 'payload line'; done"
                            };
                            tool_call_stream_for(
                                "ExecCommand",
                                serde_json::json!({"command": command}),
                                100,
                            )
                        } else {
                            tool_call_stream(100)
                        }
                    } else {
                        text_stream("SERVER_OVERFLOW_RECOVERED", 100)
                    }
                }
                ServerMode::OverflowGrowth => {
                    if !streaming {
                        if body.contains("[COMPACTION]") {
                            long_summary_response()
                        } else {
                            summary_response()
                        }
                    } else if body.contains(mock::CONTEXT_OVERFLOW_STREAM_TRIGGER)
                        && body.contains("\"role\":\"tool\"")
                    {
                        overflow_response()
                    } else if !body.contains("\"role\":\"tool\"")
                        && !body.contains("Earlier context compacted")
                    {
                        tool_call_stream(100)
                    } else {
                        text_stream("SERVER_OVERFLOW_GROWTH_RETRIED", 100)
                    }
                }
            };
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    TestServer { port, bodies }
}

pub(crate) async fn new_agent_for(
    server: &TestServer,
    name: &str,
    context_window: u64,
    max_output: u64,
) -> (
    pig_core::AgentHandle,
    String,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let dir = workspace(name);
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), "known file").unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        config(
            &format!("http://127.0.0.1:{}/v1", server.port),
            ApiFormat::OpenAiChat,
            context_window,
            max_output,
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    (agent, session_id, config_path, dir, data_dir)
}

pub(crate) fn last_was_typed_overflow(events: &[Event]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            Event::Error {
                error: pig_protocol::CoreError::ContextWindowExceeded { .. },
                ..
            }
        )
    })
}

/// The workspace path is the parent of the session data directory used by the
/// fixture.
pub(crate) fn dir_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.parent().expect("workspace parent").to_path_buf()
}
