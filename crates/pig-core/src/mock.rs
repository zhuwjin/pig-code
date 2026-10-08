//! A provider simulating OpenAI Chat Completions + SSE, behavior-aligned with
//! GLM/DeepSeek: the first request returns a Read tool call (arguments in
//! chunks); once tool results are present, it returns reasoning_content + a
//! Markdown text stream. Reused by integration tests, examples, and GUI
//! self-testing.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use std::sync::atomic::{AtomicUsize, Ordering};

pub const MOCK_FILE_NAME: &str = "README.mock.md";
pub const MOCK_FILE_CONTENT: &str =
    "# mock file\n\nA known file used by the pig-core mock provider for self-testing.\n";
pub const MOCK_REASONING: &str =
    "The user asked me to read a file and summarize it. Call Read first.";
pub const MOCK_REPLY_MARKER: &str = "MOCK_REPLY_OK";

/// Scenario B: when the user message contains this marker, run the full modification chain Write → Edit → Bash → text.
pub const SCENARIO_B_TRIGGER: &str = "SCENARIO_B";
pub const SCENARIO_B_MARKER: &str = "MOCK_SCENARIO_B_OK";
pub const SCENARIO_B_FILE: &str = "src/hello.txt";
pub const SCENARIO_B_CONTENT: &str = "hello\nline2\nline3\n";
pub const SCENARIO_B_BASH_MARKER: &str = "MOCK_BASH_OK";

/// Thinking ticker demo scenario: when this marker is present, stream
/// **variable-speed** multi-line thinking directly (no tool calls) — slow long
/// lines (to watch tail-pinned horizontal scrolling/fade) → rapid-fire short
/// lines (to watch vertical-scroll queueing/skipping) → steady finish (to
/// watch the line-by-line vertical scroll rhythm) → text containing the
/// marker. Does not use the unified 50ms write loop (each chunk carries its
/// own delay).
pub const SCENARIO_TICKER_TRIGGER: &str = "TICKER_SCENARIO";
pub const TICKER_MARKER: &str = "MOCK_TICKER_DONE";

/// TodoList scenario: when this marker is present, run TodoList write → text (for testing todo persistence).
pub const TODO_SCENARIO_TRIGGER: &str = "TODO_SCENARIO";
pub const TODO_SCENARIO_MARKER: &str = "MOCK_TODO_OK";
pub const TODO_SCENARIO_ITEM: &str = "persistent todo item";

/// AskUserQuestion scenario: marker present and no tool results → an
/// AskUserQuestion call (1 question, 2 options); with tool results → text
/// containing the marker.
pub const SCENARIO_Q_TRIGGER: &str = "SCENARIO_Q";
pub const MOCK_Q_MARKER: &str = "MOCK_QUESTION_OK";

/// Dangerous command scenario: when this marker is present, advance by the
/// number of tool results — 0/1 results both send a dangerous Bash (the second
/// verifies that AlwaysAllow does not remember dangerous commands), ≥2 → text
/// containing the marker. The command is mkfs: it hits the blocklist (the mkfs
/// command slot) yet is harmless to run — macOS has no such command (exit 127),
/// and a no-argument call on Linux only prints usage without touching the disk.
pub const SCENARIO_DANGER_TRIGGER: &str = "DANGER_SCENARIO";
pub const DANGER_COMMAND: &str = "mkfs";
pub const DANGER_MARKER: &str = "MOCK_DANGER_DONE";

/// Subject granularity scenario (AlwaysAllow refinement check): Write a →
/// Write a → Write b → Bash echo → Bash echo → Bash ls → text. The second call
/// with the same subject should not trigger another approval popup.
pub const SCENARIO_SUBJECT_TRIGGER: &str = "SUBJECT_SCENARIO";
pub const SUBJECT_MARKER: &str = "MOCK_SUBJECT_DONE";
pub const SUBJECT_FILE_A: &str = "subject_a.txt";
pub const SUBJECT_FILE_B: &str = "subject_b.txt";

/// Read-only command scenario (AutoEdit pass-through check): Bash ls → text. ls is inside the read-only allowlist.
pub const SCENARIO_READONLY_TRIGGER: &str = "READONLY_SCENARIO";
/// Out-of-workspace approval scenarios (tests/fs_outside.rs): the message
/// carries the absolute target path as the whitespace-delimited token right
/// after the trigger — tests use forward-slash paths, which survive the raw
/// JSON body without escaping.
pub const SCENARIO_OUTSIDE_READ_TRIGGER: &str = "SCENARIO_OUTSIDE_READ";
pub const SCENARIO_OUTSIDE_WRITE_TRIGGER: &str = "SCENARIO_OUTSIDE_WRITE";
/// Mid-turn variant: TWO outside reads in one turn (round 0 and round 1),
/// done text from round 2 — used to prove an Op::SetFsAccess flip issued
/// while the turn is running takes effect on the very next tool call
pub const SCENARIO_OUTSIDE_READ2_TRIGGER: &str = "SCENARIO_READ2_OUTSIDE";
pub const READONLY_MARKER: &str = "MOCK_READONLY_DONE";

/// Slow command scenario (verifies the stop-while-tool-running path): Bash
/// sleep 30 → text finish. Intended for interruption cases; do not use it in
/// non-interrupting cases (it really waits 30s).
pub const SCENARIO_SLOW_TRIGGER: &str = "SLOW_BASH_SCENARIO";
pub const SLOW_MARKER: &str = "MOCK_SLOW_DONE";

/// Plan exit scenario (ExitPlanMode check): 0 results → ExitPlanMode; 1 result
/// containing "Plan approved" (user Allow) → Write plan_exit.txt; otherwise a
/// text finish (a Reject result lacks the approval text → finish directly).
pub const SCENARIO_PLAN_EXIT_TRIGGER: &str = "PLAN_EXIT_SCENARIO";
pub const PLAN_EXIT_MARKER: &str = "MOCK_PLAN_EXIT_DONE";
pub const PLAN_EXIT_FILE: &str = "plan_exit.txt";

/// Plan enter/restore scenario (EnterPlanMode check): EnterPlanMode → Write
/// (should be hard-rejected by Plan) → ExitPlanMode → Write (executed after the
/// original mode is restored) → text.
pub const SCENARIO_PLAN_ENTER_TRIGGER: &str = "PLAN_ENTER_SCENARIO";
pub const PLAN_ENTER_MARKER: &str = "MOCK_PLAN_ENTER_DONE";
pub const PLAN_ENTER_FILE: &str = "plan_enter.txt";

/// Plan file semantics scenario: bare ExitPlanMode (no plan argument → core
/// reads the plan file). After approval (result contains "Plan approved") →
/// Write plan_file_exec.txt to verify execution.
pub const SCENARIO_PLAN_FILE_TRIGGER: &str = "PLAN_FILE_SCENARIO";
pub const PLAN_FILE_EXEC_FILE: &str = "plan_file_exec.txt";

/// Plan write gating scenario: Write to the plans directory (pass-through) → Write a normal file (should be hard-rejected) → text.
pub const SCENARIO_PLAN_WRITE_GATE_TRIGGER: &str = "PLAN_WRITE_GATE_SCENARIO";

/// Plan file semantics scenario: bare ExitPlanMode ({} without a plan argument); after approval, a Write follows.
fn plan_file_scenario_response(body: &str, tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks("call_pf_1", "ExitPlanMode", "{}", None),
        1 if body.contains("Plan approved") => tool_call_chunks(
            "call_pf_2",
            "Write",
            &serde_json::json!({"path": PLAN_FILE_EXEC_FILE, "content": "executed\n"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": PLAN_EXIT_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Plan write gating scenario: first write the plans directory (pass-through), then a normal file (should be hard-rejected).
fn plan_write_gate_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_pg_1",
            "Write",
            &serde_json::json!({"path": ".pigcode/plans/plan-gate.md", "content": "gate plan\n"})
                .to_string(),
            None,
        ),
        1 => tool_call_chunks(
            "call_pg_2",
            "Write",
            &serde_json::json!({"path": "other.txt", "content": "x\n"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": PLAN_EXIT_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Media tool scenario (ReadMediaFile check): 0 results → ReadMediaFile
/// pic.png → text. For capability gating (with input_image=false the session
/// layer errors out without executing) and the image-into-context pipeline.
pub const SCENARIO_MEDIA_TRIGGER: &str = "MEDIA_SCENARIO";
pub const MEDIA_MARKER: &str = "MOCK_MEDIA_DONE";

/// Subagent scenario (parent side): when this marker is present, send an Agent
/// tool call; the first word after the trigger = behavior token
/// (GREP/WRITE/BASH/LOOP/LONG/BG/BASHBG/BGSTOP/RESUME/RESUME_UNKNOWN/RESUME_RUNNING),
/// optional second word = subagent_type (default: BASH/LOOP/LONG →
/// general-purpose, otherwise explore). Once the Agent result returns, the
/// parent finishes with text.
pub const SUBAGENT_TRIGGER: &str = "SUBAGENT_SCENARIO";
pub const SUBAGENT_PARENT_DONE: &str = "MOCK_SUBAGENT_PARENT_DONE";
/// Marker in the subagent's final conclusion text (should be carried back in the parent-side Agent result)
pub const SUBAGENT_CHILD_DONE: &str = "MOCK_SUBAGENT_CHILD_DONE";
/// Marker in the subagent's new conclusion after a resume run
pub const SUBAGENT_RESUMED_DONE: &str = "MOCK_SUBAGENT_RESUMED_DONE";
/// Behavior token prefix for subagent prompts (used to recognize child-side requests; spliced into Agent.prompt by the parent side)
const SUBAGENT_CHILD_PREFIX: &str = "SUBAGENT_CHILD:";

/// Start an independent thread running a tokio runtime that serves mock SSE; returns the listening port.
pub fn start_mock_server() -> u16 {
    start_mock_server_with_log().0
}

/// Same as above, but also returns a request body log (for test assertions such as reasoning_params merge).
pub fn start_mock_server_with_log() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let port = start_mock_server_inner(Some(log.clone()));
    (port, log)
}

fn start_mock_server_inner(log: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>) -> u16 {
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
                tokio::spawn(handle_connection(stream, log));
            }
        });
    });
    port_rx.recv().expect("mock server port")
}

fn sse_chunk(delta: serde_json::Value, finish_reason: Option<&str>) -> String {
    let chunk = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
    });
    format!("data: {chunk}\n\n")
}

/// When usage: Some((prompt, completion, total)), it is merged into the final
/// chunk carrying finish_reason (the provider finishes upon seeing
/// finish_reason, so usage must arrive in the same chunk, aligned with the
/// real API)
fn tool_call_chunks(
    call_id: &str,
    name: &str,
    arguments: &str,
    usage: Option<(u64, u64, u64)>,
) -> Vec<String> {
    // The split point is taken on a char boundary (a Chinese argument cut mid multi-byte char would panic)
    let mut half = arguments.len() / 2;
    while !arguments.is_char_boundary(half) {
        half += 1;
    }
    let usage_json = usage
        .map(|(prompt, completion, total)| {
            serde_json::json!({"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": total})
        })
        .unwrap_or_else(|| serde_json::json!(null));
    vec![
        sse_chunk(
            serde_json::json!({"tool_calls": [{
                "index": 0,
                "id": call_id,
                "function": {"name": name, "arguments": &arguments[..half]},
            }]}),
            None,
        ),
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": [{
                        "index": 0,
                        "function": {"arguments": &arguments[half..]},
                    }]},
                    "finish_reason": "tool_calls",
                }],
                "usage": usage_json,
            })
        ),
    ]
}

fn tool_call_response() -> Vec<String> {
    let reasoning: Vec<String> = MOCK_REASONING
        .chars()
        .collect::<Vec<_>>()
        .chunks(6)
        .map(|c| c.iter().collect::<String>())
        .map(|delta| sse_chunk(serde_json::json!({"reasoning_content": delta}), None))
        .collect();
    let arguments = format!("{{\"path\": \"{MOCK_FILE_NAME}\"}}");
    let mut chunks = reasoning;
    // Intermediate steps also report usage (the real API includes it on every
    // request): the usage watermark should take the last step's 142, while the
    // turn accumulation (turn_stats) is the sum of all steps
    chunks.extend(tool_call_chunks(
        "call_mock_1",
        "Read",
        &arguments,
        Some((60, 10, 70)),
    ));
    chunks
}

/// Variable-speed script for the thinking ticker demo scenario: returns a
/// sequence of (pre-chunk delay ms, thinking chunk). Rhythm design (paired
/// with the UI vertical-scroll state machine's 800ms interval):
/// - Slow long lines: watch the same line refresh in place chunk by chunk; the
///   second line is deliberately overlong to watch tail-pinned horizontal
///   scrolling and the left-edge fade appear
/// - Rapid-fire short lines (interval <800ms): watch queue throttling and
///   middle-entry skipping
/// - Steady finish: each line spaced >800ms to watch the vertical scroll line
///   by line
fn ticker_scenario_script() -> Vec<(u64, String)> {
    // Slow drip: split text into size-char chunks (on char boundaries, to avoid cutting mid multi-byte char)
    fn drip(pieces: &mut Vec<(u64, String)>, text: &str, size: usize, ms: u64, newline: bool) {
        let chars: Vec<char> = text.chars().collect();
        for piece in chars.chunks(size) {
            pieces.push((ms, piece.iter().collect()));
        }
        if newline {
            pieces.push((0, "\n".to_string()));
        }
    }
    let mut pieces: Vec<(u64, String)> = Vec::new();
    drip(
        &mut pieces,
        "先理解需求：用户想看到思考过程逐行滚动展示的效果，我先把问题拆开，从渲染节奏和节流两头看。",
        4,
        130,
        true,
    );
    drip(
        &mut pieces,
        "这一行故意写得特别特别长，用来验证滚动行钉尾之后旧内容向左移出、左缘渐隐遮罩随之出现，横滚与纵滚叠在一起时互不打架，CJK 与标点混排也顺便过一遍，再继续加长一截确保任何窗口宽度下都能溢出，尾部再补一段长尾说明文字，让横向溢出在任何主题下都看得清清楚楚。",
        5,
        100,
        true,
    );
    // Rapid-fire six lines (each a single chunk, ~180ms apart, all falling
    // inside the dwell period → queueing/skipping). The six CJK fast-line
    // strings below (numbered fast line 1..6) are functional demo data.
    for word in [
        "快速行一",
        "快速行二",
        "快速行三",
        "快速行四",
        "快速行五",
        "快速行六",
    ] {
        pieces.push((180, format!("{word}\n")));
    }
    // Steady finish: three lines each dripped over ~1s, spaced >800ms apart, to
    // watch the vertical scroll line by line
    for line in [
        "连发结束，恢复逐行停留的节奏。",
        "再滚一行，确认节奏稳定。",
        "收尾前再确认一次：纵滚、钉尾、渐隐都正常。",
    ] {
        drip(&mut pieces, line, 3, 120, true);
        pieces.push((900, String::new()));
    }
    pieces
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

/// Echo the message count (for testing history rebuilding after resume): triggered when the user message contains ECHO_HISTORY.
fn echo_history_response(body: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let count = parsed["messages"]
        .as_array()
        .map(|msgs| msgs.len())
        .unwrap_or(0);
    let text = format!("HISTORY_COUNT:{count}");
    vec![
        sse_chunk(serde_json::json!({"content": text}), None),
        sse_chunk(serde_json::json!({}), Some("stop")),
    ]
}

/// Scenario B advances by the number of tool results in history: 0→Write,
/// 1→Edit, 2→Bash, ≥3→text. The file parameter avoids multiple self-test
/// sessions mutating the same file and interfering with each other.
fn outside_path_after(body: &str, trigger: &str) -> Option<String> {
    // The trigger sits inside the JSON-escaped user message: the path token
    // ends at the closing quote, not only at whitespace
    body.split(trigger)
        .nth(1)?
        .split(|c: char| c.is_whitespace() || c == '"')
        .find(|tok| !tok.is_empty())
        .map(str::to_string)
}

fn outside_text_chunks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(9)
        .map(|piece| {
            sse_chunk(
                serde_json::json!({"content": piece.iter().collect::<String>()}),
                None,
            )
        })
        .collect()
}

fn outside_read_response(path: &str, tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_outside_read",
            "Read",
            &serde_json::json!({"path": path}).to_string(),
            None,
        ),
        _ => outside_text_chunks(&format!("OUTSIDE_READ_DONE {path}")),
    }
}

fn outside_read2_response(path: &str, tool_results: usize) -> Vec<String> {
    match tool_results {
        0 | 1 => tool_call_chunks(
            "call_outside_read2",
            "Read",
            &serde_json::json!({"path": path}).to_string(),
            None,
        ),
        _ => outside_text_chunks(&format!("OUTSIDE_READ2_DONE {path}")),
    }
}

fn outside_write_response(path: &str, tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_outside_write",
            "Write",
            &serde_json::json!({"path": path, "content": "outside write"}).to_string(),
            None,
        ),
        _ => outside_text_chunks(&format!("OUTSIDE_WRITE_DONE {path}")),
    }
}

fn scenario_b_response(tool_results: usize, file: &str) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_b_write",
            "Write",
            &serde_json::json!({"path": file, "content": SCENARIO_B_CONTENT}).to_string(),
            None,
        ),
        1 => tool_call_chunks(
            "call_b_edit",
            "Edit",
            &serde_json::json!({"path": file, "old_string": "line2", "new_string": "LINE2"})
                .to_string(),
            None,
        ),
        2 => tool_call_chunks(
            "call_b_bash",
            "Bash",
            // printf is not in the read-only allowlist (echo is): AutoEdit approval semantics are covered by this command
            &serde_json::json!({"command": format!("printf '%s\\n' {SCENARIO_B_BASH_MARKER}")})
                .to_string(),
            None,
        ),
        _ => {
            let markdown = format!(
                "Done: created `{SCENARIO_B_FILE}`, edited one line, ran echo.\n\n**Result**: {SCENARIO_B_MARKER}\n"
            );
            let chars: Vec<char> = markdown.chars().collect();
            let mut chunks: Vec<String> = chars
                .chunks(9)
                .map(|piece| {
                    let delta: String = piece.iter().collect();
                    sse_chunk(serde_json::json!({"content": delta}), None)
                })
                .collect();
            // The final step carries usage (the real API includes usage at the
            // stream end; the watermark bar/turn stats rely on it)
            chunks.push(format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chatcmpl-mock",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 42, "total_tokens": 142},
                })
            ));
            chunks
        }
    }
}

/// Dangerous command scenario: 0/1 tool results both send a dangerous Bash, ≥2 → text finish.
fn danger_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 | 1 => tool_call_chunks(
            if tool_results == 0 {
                "call_danger_1"
            } else {
                "call_danger_2"
            },
            "Bash",
            &serde_json::json!({"command": DANGER_COMMAND}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": DANGER_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Subject granularity scenario: the second call with the same subject does not pop up (Write a twice in a row), a different subject pops up.
fn subject_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 | 1 => tool_call_chunks(
            if tool_results == 0 {
                "call_subj_w1"
            } else {
                "call_subj_w2"
            },
            "Write",
            &serde_json::json!({"path": SUBJECT_FILE_A, "content": format!("v{}\n", tool_results)})
                .to_string(),
            None,
        ),
        2 => tool_call_chunks(
            "call_subj_w3",
            "Write",
            &serde_json::json!({"path": SUBJECT_FILE_B, "content": "b\n"}).to_string(),
            None,
        ),
        3 | 4 => tool_call_chunks(
            if tool_results == 3 {
                "call_subj_e1"
            } else {
                "call_subj_e2"
            },
            "Bash",
            &serde_json::json!({"command": format!("echo SUBJ_{}", tool_results)}).to_string(),
            None,
        ),
        5 => tool_call_chunks(
            "call_subj_ls",
            "Bash",
            &serde_json::json!({"command": "ls"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": SUBJECT_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Read-only command scenario: Bash ls → text.
fn readonly_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_ro_ls",
            "Bash",
            &serde_json::json!({"command": "ls"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": READONLY_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Slow command scenario (verifies the stop-while-tool-running path): Bash
/// sleep 30 → text finish. Intended for interruption cases; do not use it in
/// non-interrupting cases (it really waits 30s).
fn slow_bash_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_slow_bash",
            "Bash",
            &serde_json::json!({"command": "sleep 30"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": SLOW_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Plan exit scenario: advances by tool result count; after Allow (result contains "Plan approved") a Write follows.
fn plan_exit_scenario_response(body: &str, tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_pe_1",
            "ExitPlanMode",
            &serde_json::json!({"plan": "Step 1: create plan_exit.txt to verify execution"})
                .to_string(),
            None,
        ),
        1 if body.contains("Plan approved") => tool_call_chunks(
            "call_pe_2",
            "Write",
            &serde_json::json!({"path": PLAN_EXIT_FILE, "content": "executed\n"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": PLAN_EXIT_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Plan enter/restore scenario: advances purely by tool result count (a Write hard-rejected by Plan still counts as a step).
fn plan_enter_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_pn_1",
            "EnterPlanMode",
            &serde_json::json!({"reason": "Large change scope; plan first"}).to_string(),
            None,
        ),
        1 => tool_call_chunks(
            "call_pn_2",
            "Write",
            &serde_json::json!({"path": PLAN_ENTER_FILE, "content": "from-plan\n"}).to_string(),
            None,
        ),
        2 => tool_call_chunks(
            "call_pn_3",
            "ExitPlanMode",
            &serde_json::json!({"plan": "Plan ready"}).to_string(),
            None,
        ),
        3 => tool_call_chunks(
            "call_pn_4",
            "Write",
            &serde_json::json!({"path": PLAN_ENTER_FILE, "content": "executed\n"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": PLAN_ENTER_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Media tool scenario: ReadMediaFile pic.png → text.
fn media_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_media_1",
            "ReadMediaFile",
            &serde_json::json!({"path": "pic.png"}).to_string(),
            None,
        ),
        _ => vec![
            sse_chunk(serde_json::json!({"content": MEDIA_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ],
    }
}

/// Text of the last user message in the request body (shared: the parent side takes the behavior token, the child side takes the prompt token)
fn last_user_text(body: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    parsed["messages"]
        .as_array()
        .and_then(|msgs| {
            msgs.iter()
                .rev()
                .find(|m| m["role"].as_str() == Some("user"))
        })
        .and_then(|m| m["content"].as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Whether a tool result has already been answered (both compact and spaced serialization forms)
fn tool_answered(body: &str, call_id: &str) -> bool {
    body.contains(&format!("\"tool_call_id\":\"{call_id}\""))
        || body.contains(&format!("\"tool_call_id\": \"{call_id}\""))
}

/// Extract "agent_id: a..."/"task_id: b..." from the request body (lines in Agent result templates/background receipts)
fn extract_marker_id(body: &str, marker: &str) -> Option<String> {
    let rest = body.split(marker).nth(1)?;
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (!id.is_empty()).then_some(id)
}

fn subagent_parent_done() -> Vec<String> {
    vec![
        sse_chunk(serde_json::json!({"content": SUBAGENT_PARENT_DONE}), None),
        sse_chunk(serde_json::json!({}), Some("stop")),
    ]
}

fn subagent_agent_call(call_id: &str, args: serde_json::Value) -> Vec<String> {
    tool_call_chunks(call_id, "Agent", &args.to_string(), None)
}

fn child_prompt(behavior: &str) -> String {
    format!("{SUBAGENT_CHILD_PREFIX}{behavior} read {MOCK_FILE_NAME} and summarize")
}

/// Subagent scenario (parent side): advances the state machine by behavior
/// token —
/// Normal tokens: call_agent_1 (foreground) → result returns → text finish;
/// BG/BASHBG: call_agent_bg (run_in_background) → running receipt → text finish;
/// SWARMBG: call_swarm_bg (AgentSwarm run_in_background, 2 items) → receipt →
/// text finish;
/// BGSTOP: background LOOP → running receipt → TaskStop → text finish;
/// RESUME: first message foreground GREP (call_agent_1), second message
/// resumes the original id (call_agent_2);
/// RESUME_UNKNOWN: resume a nonexistent id directly (call_agent_2);
/// RESUME_RUNNING: first message background LOOP (call_agent_bg), second
/// message resumes the same id (call_agent_2, should hit the conflict).
fn subagent_parent_response(body: &str) -> Vec<String> {
    let user_text = last_user_text(body);
    // Wake turn: the last user message is the synthetic <task-notification>
    // message (no trigger) → finish directly
    if !user_text.contains(SUBAGENT_TRIGGER) {
        return subagent_parent_done();
    }
    let mut tokens = user_text
        .split(SUBAGENT_TRIGGER)
        .nth(1)
        .unwrap_or("")
        .split_whitespace();
    let behavior = tokens.next().unwrap_or("GREP");
    let profile_arg = tokens.next();
    match behavior {
        "SWARMBG" => {
            if tool_answered(body, "call_swarm_bg") {
                return subagent_parent_done();
            }
            tool_call_chunks(
                "call_swarm_bg",
                "AgentSwarm",
                &serde_json::json!({
                    "prompt_template": format!("{SUBAGENT_CHILD_PREFIX}GREP handle {{{{item}}}}"),
                    "items": ["item-a", "item-b"],
                    "run_in_background": true,
                })
                .to_string(),
                None,
            )
        }
        "BG" | "BASHBG" => {
            if tool_answered(body, "call_agent_bg") {
                return subagent_parent_done();
            }
            let (child, profile) = if behavior == "BG" {
                ("GREP", "explore")
            } else {
                ("BASH", "general-purpose")
            };
            subagent_agent_call(
                "call_agent_bg",
                serde_json::json!({
                    "description": "subagent selftest delegation",
                    "prompt": child_prompt(child),
                    "subagent_type": profile_arg.unwrap_or(profile),
                    "run_in_background": true,
                }),
            )
        }
        "BGSTOP" => {
            if tool_answered(body, "call_taskstop") {
                return subagent_parent_done();
            }
            if tool_answered(body, "call_agent_bg") {
                // The background subagent is already running (LOOP never stops): send TaskStop to stop it
                let task_id =
                    extract_marker_id(body, "task_id: ").unwrap_or_else(|| "b1".to_string());
                return tool_call_chunks(
                    "call_taskstop",
                    "TaskStop",
                    &serde_json::json!({"task_id": task_id}).to_string(),
                    None,
                );
            }
            subagent_agent_call(
                "call_agent_bg",
                serde_json::json!({
                    "description": "subagent selftest delegation",
                    "prompt": child_prompt("LOOP"),
                    "subagent_type": profile_arg.unwrap_or("explore"),
                    "run_in_background": true,
                }),
            )
        }
        "RESUME" => {
            // Trigger occurrence count = messages already sent: the 1st starts
            // a new subagent, the 2nd resumes the original id
            if body.matches(SUBAGENT_TRIGGER).count() >= 2 {
                if tool_answered(body, "call_agent_2") {
                    return subagent_parent_done();
                }
                let agent_id = extract_marker_id(body, "agent_id: ")
                    .expect("resume scenario: agent_id should be in history");
                return subagent_agent_call(
                    "call_agent_2",
                    serde_json::json!({
                        "description": "subagent resume",
                        "prompt": child_prompt("RESUMED"),
                        "resume": agent_id,
                    }),
                );
            }
            if tool_answered(body, "call_agent_1") {
                return subagent_parent_done();
            }
            subagent_agent_call(
                "call_agent_1",
                serde_json::json!({
                    "description": "subagent selftest delegation",
                    "prompt": child_prompt("GREP"),
                    "subagent_type": profile_arg.unwrap_or("explore"),
                }),
            )
        }
        "RESUME_UNKNOWN" => {
            if tool_answered(body, "call_agent_2") {
                return subagent_parent_done();
            }
            subagent_agent_call(
                "call_agent_2",
                serde_json::json!({
                    "description": "resume nonexistent subagent",
                    "prompt": child_prompt("GREP"),
                    "resume": "a0-999",
                }),
            )
        }
        "RESUME_RUNNING" => {
            if body.matches(SUBAGENT_TRIGGER).count() >= 2 {
                if tool_answered(body, "call_agent_2") {
                    return subagent_parent_done();
                }
                let agent_id = extract_marker_id(body, "agent_id: ")
                    .expect("resume scenario: agent_id should be in history");
                return subagent_agent_call(
                    "call_agent_2",
                    serde_json::json!({
                        "description": "resume running subagent",
                        "prompt": child_prompt("GREP"),
                        "resume": agent_id,
                    }),
                );
            }
            if tool_answered(body, "call_agent_bg") {
                return subagent_parent_done();
            }
            subagent_agent_call(
                "call_agent_bg",
                serde_json::json!({
                    "description": "subagent selftest delegation",
                    "prompt": child_prompt("LOOP"),
                    "subagent_type": profile_arg.unwrap_or("explore"),
                    "run_in_background": true,
                }),
            )
        }
        _ => {
            if tool_answered(body, "call_agent_1") {
                return subagent_parent_done();
            }
            let default_type = match behavior {
                "BASH" | "LOOP" | "LONG" => "general-purpose",
                _ => "explore",
            };
            subagent_agent_call(
                "call_agent_1",
                serde_json::json!({
                    "description": "subagent selftest delegation",
                    "prompt": child_prompt(behavior),
                    "subagent_type": profile_arg.unwrap_or(default_type),
                }),
            )
        }
    }
}

/// Subagent scenario (child side): acts by the behavior token in the prompt
/// (the last user message) — with 0 tool results it emits a tool call (LOOP
/// always emits tool calls, forcing the parent to finish via max_turns; LONG
/// replies directly with a 33K-char long text; RESUMED replies directly with
/// the resume conclusion), otherwise it replies with the marker-bearing
/// conclusion text.
fn subagent_child_response(body: &str, tool_results: usize) -> Vec<String> {
    let user_text = last_user_text(body);
    let behavior = user_text
        .split(SUBAGENT_CHILD_PREFIX)
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("");
    let done_text = |text: String| {
        vec![
            sse_chunk(serde_json::json!({"content": text}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ]
    };
    match behavior {
        "RESUMED" => done_text(format!(
            "Subagent resumed: task done. {SUBAGENT_RESUMED_DONE}"
        )),
        "LONG" => {
            // 33K-char long text (a single repeated CJK char pads the body):
            // verifies the parent's 32K result budget truncation + full-text
            // persistence. 2000 chars per chunk (the mock sleeps 50ms per
            // chunk, so the chunk count must stay low)
            let text = format!("Subagent long result start. {}", "密".repeat(33_000));
            let chars: Vec<char> = text.chars().collect();
            let mut chunks: Vec<String> = chars
                .chunks(2000)
                .map(|piece| {
                    let delta: String = piece.iter().collect();
                    sse_chunk(serde_json::json!({"content": delta}), None)
                })
                .collect();
            chunks.push(sse_chunk(serde_json::json!({}), Some("stop")));
            chunks
        }
        "LOOP" => tool_call_chunks(
            &format!("call_child_loop_{tool_results}"),
            "Grep",
            &serde_json::json!({"pattern": "mock"}).to_string(),
            None,
        ),
        _ if tool_results == 0 => match behavior {
            "WRITE" => tool_call_chunks(
                "call_child_write",
                "Write",
                &serde_json::json!({"path": "child_write.txt", "content": "x\n"}).to_string(),
                None,
            ),
            "BASH" => tool_call_chunks(
                "call_child_bash",
                "Bash",
                &serde_json::json!({"command": "touch child_bash.txt"}).to_string(),
                None,
            ),
            _ => tool_call_chunks(
                "call_child_grep",
                "Grep",
                &serde_json::json!({"pattern": "mock"}).to_string(),
                None,
            ),
        },
        _ => done_text(format!(
            "Subagent done: task complete. {SUBAGENT_CHILD_DONE}"
        )),
    }
}

/// TodoList scenario: no TodoList call in history yet → write one; already
/// executed → text finish. Cannot count global tool results: both the request
/// body's tools declaration and history messages interfere, so directly parse
/// whether a TodoList call appears in messages.
fn todo_scenario_response(body: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let called = parsed["messages"].as_array().is_some_and(|msgs| {
        msgs.iter().any(|m| {
            m["tool_calls"].as_array().is_some_and(|calls| {
                calls
                    .iter()
                    .any(|c| c["function"]["name"].as_str() == Some("TodoList"))
            })
        })
    });
    if called {
        vec![
            sse_chunk(serde_json::json!({"content": TODO_SCENARIO_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ]
    } else {
        tool_call_chunks(
            "call_todo_1",
            "TodoList",
            &serde_json::json!({"todos": [
                {"content": format!("{TODO_SCENARIO_ITEM} 1"), "status": "done"},
                {"content": format!("{TODO_SCENARIO_ITEM} 2"), "status": "in_progress"}
            ]})
            .to_string(),
            None,
        )
    }
}

/// AskUserQuestion scenario: no AskUserQuestion call in history yet → ask;
/// already executed → text finish. (Same as the TodoList scenario: scan
/// messages directly; cannot count global tool results)
fn question_scenario_response(body: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let called = parsed["messages"].as_array().is_some_and(|msgs| {
        msgs.iter().any(|m| {
            m["tool_calls"].as_array().is_some_and(|calls| {
                calls
                    .iter()
                    .any(|c| c["function"]["name"].as_str() == Some("AskUserQuestion"))
            })
        })
    });
    if called {
        vec![
            sse_chunk(serde_json::json!({"content": MOCK_Q_MARKER}), None),
            sse_chunk(serde_json::json!({}), Some("stop")),
        ]
    } else {
        tool_call_chunks(
            "call_q_1",
            "AskUserQuestion",
            &serde_json::json!({"questions": [
                {
                    "question": "Choose an implementation approach",
                    "header": "Approach",
                    "options": [
                        {"label": "Option A", "description": "Simple and direct"},
                        {"label": "Option B", "description": "More complete but complex"}
                    ]
                },
                {
                    "question": "Should tests run?",
                    "header": "Tests",
                    "options": [
                        {"label": "Yes", "description": "Run after changes"},
                        {"label": "No", "description": "Skip for now"}
                    ]
                }
            ]})
            .to_string(),
            None,
        )
    }
}

fn text_response() -> Vec<String> {
    let markdown = format!(
        "## File summary\n\nContents of `{MOCK_FILE_NAME}`:\n\n```text\nA known file used by the pig-core mock provider for self-testing.\n```\n\n**Conclusion**: {MOCK_REPLY_MARKER}\n"
    );
    let mut chunks = vec![sse_chunk(
        serde_json::json!({"reasoning_content": "The tool returned the file content; composing the answer."}),
        None,
    )];
    let chars: Vec<char> = markdown.chars().collect();
    for piece in chars.chunks(9) {
        let delta: String = piece.iter().collect();
        chunks.push(sse_chunk(serde_json::json!({"content": delta}), None));
    }
    chunks.push(format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "mock-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 42, "total_tokens": 142},
        })
    ));
    chunks
}

pub const SCENARIO_C_TRIGGER: &str = "SCENARIO_C";
pub const PLAN_MARKER: &str = "MOCK_PLAN_OK";
pub const SUMMARY_MARKER: &str = "MOCK_SUMMARY_OK";
/// Mock title for the session auto-naming sidecar (for selftest assertions)
pub const MOCK_TITLE: &str = "auto-title selftest";

/// Non-streaming auto-naming response: {"title": MOCK_TITLE} (same content for
/// both API formats). Request recognition is in session::TITLE_PROMPT_MARKER
async fn write_title_response(stream: &mut tokio::net::TcpStream, anthropic: bool) -> bool {
    let content = format!("{{\"title\":\"{MOCK_TITLE}\"}}");
    let json = if anthropic {
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
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        payload.len(),
        payload
    );
    stream.write_all(resp.as_bytes()).await.is_ok()
}

/// Non-streaming response (compact summary). FAIL_COMPACT triggers a 500 to test the fallback path.
async fn write_json_response(stream: &mut tokio::net::TcpStream, body: &str) -> bool {
    if body.contains("FAIL_COMPACT") {
        let resp = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n\r\n{}";
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
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        payload.len(),
        payload
    );
    stream.write_all(resp.as_bytes()).await.is_ok()
}

/// Scenario C: plan mode (kimi file semantics) — Write a plan file →
/// ExitPlanMode → after approval, the scenario B tool chain follows (same as
/// the live full pipeline: Write card → plan line → approval panel → work
/// starts)
fn scenario_c_response(tool_results: usize) -> Vec<String> {
    let plan = format!(
        "## Execution plan\n\n1. Create `src/hello.txt` with three lines\n2. Uppercase the second line\n3. Run echo to verify\n\n**Plan ready**: {PLAN_MARKER}\n"
    );
    match tool_results {
        0 => tool_call_chunks(
            "call_c_1",
            "Write",
            &serde_json::json!({"path": ".pigcode/plans/plan-mock.md", "content": plan})
                .to_string(),
            None,
        ),
        1 => tool_call_chunks(
            "call_c_2",
            "ExitPlanMode",
            &serde_json::json!({"plan": plan}).to_string(),
            None,
        ),
        _ => scenario_b_response(tool_results - 2, "src/hello_plan.txt"),
    }
}

/// ECHO_USAGE <n>: text reply + the given total_tokens (for testing auto compact triggering).
fn echo_usage_response(body: &str) -> Vec<String> {
    let usage: u64 = {
        let marker = "ECHO_USAGE";
        let pos = body.find(marker).map(|p| p + marker.len()).unwrap_or(0);
        body[pos..]
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    };
    vec![
        sse_chunk(serde_json::json!({"content": "usage noted"}), None),
        format!(
            "data: {}

",
            serde_json::json!({
                "id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": 0, "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": usage},
            })
        ),
    ]
}

async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    log: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
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
    let path = head_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
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
    static FAIL_ONCE_500: AtomicUsize = AtomicUsize::new(1);
    if body.contains("FAIL_ONCE_500")
        && FAIL_ONCE_500
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
    {
        let resp = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n\r\n{}";
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
        return;
    }

    let anthropic = path.ends_with("/messages");
    let tool_results = body.matches("\"role\":\"tool\"").count()
        + body.matches("\"role\": \"tool\"").count()
        + body.matches("tool_result").count();

    // Non-streaming = auto naming / compact summary / connectivity test (must branch before the SSE response head)
    if body.contains("\"stream\":false") {
        let ok = if body.contains(crate::session::TITLE_PROMPT_MARKER) {
            write_title_response(&mut stream, anthropic).await
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

    let response_head =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    if stream.write_all(response_head.as_bytes()).await.is_err() {
        return;
    }
    // Thinking ticker demo scenario: variable-speed emission (each chunk
    // carries its own delay), not the unified 50ms write loop
    if !anthropic && body.contains(SCENARIO_TICKER_TRIGGER) {
        write_ticker_scenario(&mut stream).await;
        return;
    }
    let chunks = if anthropic {
        anthropic_chunks(&body, tool_results)
    } else if body.contains("ECHO_USAGE") {
        echo_usage_response(&body)
    } else if body.contains("ECHO_HISTORY") {
        echo_history_response(&body)
    } else if body.contains(SCENARIO_C_TRIGGER) {
        scenario_c_response(tool_results)
    } else if body.contains(TODO_SCENARIO_TRIGGER) {
        todo_scenario_response(&body)
    } else if body.contains(SCENARIO_Q_TRIGGER) {
        question_scenario_response(&body)
    } else if body.contains(SCENARIO_DANGER_TRIGGER) {
        danger_scenario_response(tool_results)
    } else if body.contains(SCENARIO_SUBJECT_TRIGGER) {
        subject_scenario_response(tool_results)
    } else if body.contains(SCENARIO_READONLY_TRIGGER) {
        readonly_scenario_response(tool_results)
    } else if body.contains(SCENARIO_SLOW_TRIGGER) {
        slow_bash_scenario_response(tool_results)
    } else if body.contains(SCENARIO_PLAN_EXIT_TRIGGER) {
        plan_exit_scenario_response(&body, tool_results)
    } else if body.contains(SCENARIO_PLAN_ENTER_TRIGGER) {
        plan_enter_scenario_response(tool_results)
    } else if body.contains(SCENARIO_PLAN_FILE_TRIGGER) {
        plan_file_scenario_response(&body, tool_results)
    } else if body.contains(SCENARIO_PLAN_WRITE_GATE_TRIGGER) {
        plan_write_gate_response(tool_results)
    } else if body.contains(SCENARIO_MEDIA_TRIGGER) {
        media_scenario_response(tool_results)
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
    } else if body.contains(SCENARIO_B_TRIGGER) {
        scenario_b_response(tool_results, SCENARIO_B_FILE)
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

// ---------------- Anthropic format ----------------

fn a_sse(event: &str, data: serde_json::Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn anthropic_tool_call(out: &mut Vec<String>, index: usize, id: &str, name: &str, arguments: &str) {
    out.push(a_sse(
        "content_block_start",
        serde_json::json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": id, "name": name}}),
    ));
    // The split point is taken on a char boundary (a Chinese argument cut mid multi-byte char would panic)
    let mut half = arguments.len() / 2;
    while !arguments.is_char_boundary(half) {
        half += 1;
    }
    out.push(a_sse(
        "content_block_delta",
        serde_json::json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": &arguments[..half]}}),
    ));
    out.push(a_sse(
        "content_block_delta",
        serde_json::json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": &arguments[half..]}}),
    ));
    out.push(a_sse(
        "content_block_stop",
        serde_json::json!({"type": "content_block_stop", "index": index}),
    ));
}

/// Anthropic scenario dispatch: no tool_result → Read tool call; otherwise a
/// text reply. With SCENARIO_B_TRIGGER it runs the Write→Edit→Bash chain.
fn anthropic_chunks(body: &str, tool_results: usize) -> Vec<String> {
    if body.contains(SCENARIO_B_TRIGGER) {
        return anthropic_scenario_b(tool_results);
    }
    if body.contains(SCENARIO_Q_TRIGGER) {
        return anthropic_question_scenario(tool_results);
    }
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
    )];

    // Thinking block
    out.push(a_sse(
        "content_block_start",
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking"}}),
    ));
    for piece in MOCK_REASONING.chars().collect::<Vec<_>>().chunks(8) {
        let thinking: String = piece.iter().collect();
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": thinking}}),
        ));
    }
    out.push(a_sse(
        "content_block_stop",
        serde_json::json!({"type": "content_block_stop", "index": 0}),
    ));

    if tool_results == 0 {
        // Read tool call with arguments in chunks (split point on a char boundary)
        let arguments = format!("{{\"path\": \"{MOCK_FILE_NAME}\"}}");
        let mut half = arguments.len() / 2;
        while !arguments.is_char_boundary(half) {
            half += 1;
        }
        out.push(a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "call_mock_1", "name": "Read"}}),
        ));
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": &arguments[..half]}}),
        ));
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": &arguments[half..]}}),
        ));
        out.push(a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 1}),
        ));
        out.push(a_sse(
            "message_delta",
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
        ));
    } else {
        let markdown = format!(
            "## File summary\n\nContents of `{MOCK_FILE_NAME}`:\n\n```text\nA known file used by the pig-core mock provider for self-testing.\n```\n\n**Conclusion**: {MOCK_REPLY_MARKER}\n"
        );
        out.push(a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text"}}),
        ));
        for piece in markdown.chars().collect::<Vec<_>>().chunks(9) {
            let text: String = piece.iter().collect();
            out.push(a_sse(
                "content_block_delta",
                serde_json::json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": text}}),
            ));
        }
        out.push(a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 1}),
        ));
        out.push(a_sse(
            "message_delta",
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 42}}),
        ));
    }
    let _ = body;
    out
}

fn anthropic_scenario_b(tool_results: usize) -> Vec<String> {
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
    )];
    match tool_results {
        0 => anthropic_tool_call(&mut out, 0, "call_b_write", "Write",
            &serde_json::json!({"path": SCENARIO_B_FILE, "content": SCENARIO_B_CONTENT}).to_string()),
        1 => anthropic_tool_call(&mut out, 0, "call_b_edit", "Edit",
            &serde_json::json!({"path": SCENARIO_B_FILE, "old_string": "line2", "new_string": "LINE2"}).to_string()),
        2 => anthropic_tool_call(&mut out, 0, "call_b_bash", "Bash",
            // Same rationale as the OpenAI branch: printf is not in the read-only allowlist (echo is)
            &serde_json::json!({"command": format!("printf '%s\\n' {SCENARIO_B_BASH_MARKER}")}).to_string()),
        _ => {
            let text = format!("Scenario B done. **Result**: {SCENARIO_B_MARKER}\n");
            out.push(a_sse(
                "content_block_start",
                serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            ));
            out.push(a_sse(
                "content_block_delta",
                serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            ));
            out.push(a_sse(
                "content_block_stop",
                serde_json::json!({"type": "content_block_stop", "index": 0}),
            ));
        }
    }
    out.push(a_sse(
        "message_delta",
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": if tool_results >= 3 { "end_turn" } else { "tool_use" }}, "usage": {"output_tokens": 42}}),
    ));
    out
}

/// Anthropic AskUserQuestion scenario: no tool_result → the question tool call; otherwise text containing the marker.
fn anthropic_question_scenario(tool_results: usize) -> Vec<String> {
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
    )];
    if tool_results == 0 {
        let arguments = serde_json::json!({"questions": [
            {
                "question": "Choose an implementation approach",
                "header": "Approach",
                "options": [
                    {"label": "Option A", "description": "Simple and direct"},
                    {"label": "Option B", "description": "More complete but complex"}
                ]
            },
            {
                "question": "Should tests run?",
                "header": "Tests",
                "options": [
                    {"label": "Yes", "description": "Run after changes"},
                    {"label": "No", "description": "Skip for now"}
                ]
            }
        ]})
        .to_string();
        anthropic_tool_call(&mut out, 0, "call_q_1", "AskUserQuestion", &arguments);
    } else {
        out.push(a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ));
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": MOCK_Q_MARKER}}),
        ));
        out.push(a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ));
    }
    out.push(a_sse(
        "message_delta",
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": if tool_results == 0 { "tool_use" } else { "end_turn" }}, "usage": {"output_tokens": 20}}),
    ));
    out
}

async fn write_json_response_anthropic(stream: &mut tokio::net::TcpStream, body: &str) -> bool {
    if body.contains("FAIL_COMPACT") {
        let resp = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n\r\n{}";
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
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        payload.len(),
        payload
    );
    stream.write_all(resp.as_bytes()).await.is_ok()
}
