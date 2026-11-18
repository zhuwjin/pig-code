//! SSE/event frame builders shared by the mock scenarios.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::scenarios::*;

pub(super) fn sse_chunk(delta: serde_json::Value, finish_reason: Option<&str>) -> String {
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
/// chunk carrying finish_reason. USAGE_TAIL_TRIGGER separately covers the
/// standard variant where a usage-only chunk follows the finish chunk.
pub(super) fn tool_call_chunks(
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

pub(super) fn tool_call_response() -> Vec<String> {
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
pub(super) fn ticker_scenario_script() -> Vec<(u64, String)> {
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

pub(super) fn outside_text_chunks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut chunks: Vec<String> = chars
        .chunks(9)
        .map(|piece| {
            sse_chunk(
                serde_json::json!({"content": piece.iter().collect::<String>()}),
                None,
            )
        })
        .collect();
    chunks.push(sse_chunk(serde_json::json!({}), Some("stop")));
    chunks
}

/// The Write arguments JSON of the streaming-args scenario split into raw-JSON
/// fragments: every fragment after the first starts with the escaped `\n`
/// that ends the previous content line, so each content line flushes as its
/// own live delta (the last fragment closes the string and the object)
pub(crate) fn stream_args_fragments() -> Vec<String> {
    let arguments = serde_json::json!({
        "path": STREAM_ARGS_FILE,
        "content": STREAM_ARGS_CONTENT,
    })
    .to_string();
    // The serialized string has `l1\nl2\nl3\n` as escaped \n pairs; split
    // right before each `\n` escape so each fragment carries one line break
    let mut fragments = Vec::new();
    let mut rest = arguments.as_str();
    while let Some(pos) = rest.find("\\n") {
        let cut = pos + 2; // include the escape in this fragment
        fragments.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        fragments.push(rest.to_string());
    }
    fragments
}

/// Chat-completions chunks: id+name in the first chunk, then one chunk per
/// fragment, finish_reason + usage on the last
pub(super) fn stream_args_chat_chunks() -> Vec<String> {
    let fragments = stream_args_fragments();
    let mut out = vec![sse_chunk(
        serde_json::json!({"tool_calls": [{
            "index": 0,
            "id": "call_stream_args",
            "function": {"name": "Write", "arguments": fragments[0]},
        }]}),
        None,
    )];
    for fragment in &fragments[1..] {
        out.push(sse_chunk(
            serde_json::json!({"tool_calls": [{
                "index": 0,
                "function": {"arguments": fragment},
            }]}),
            None,
        ));
    }
    out.push(format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "mock-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 60, "completion_tokens": 12, "total_tokens": 72},
        })
    ));
    out
}

/// The demo file's content: 40 lines (the streaming box outgrows its 240px
/// cap early, so the tail pin + scrollbar clearly show); Edit's old_string
/// matches lines 07..=14 exactly. No ASCII quotes anywhere (the naive
/// fragment splitter cuts on the `","` field boundary)
pub(super) fn stream_all_write_content() -> String {
    let mut lines: Vec<String> = (1..=40)
        .map(|n| {
            if n == 20 {
                "line 20 demo —— 中文混排 wide chars".to_string()
            } else {
                format!("line {n:02} demo")
            }
        })
        .collect();
    lines.push(String::new()); // trailing newline
    lines.join("\n")
}

/// Field-ordered arguments JSON for the demo calls (serde_json's Map sorts
/// keys without preserve_order — `content` would land BEFORE `path` and the
/// pending card would grow content with no file chip; the demo controls the
/// emission order: path/identity fields first, content last, as a model does)
pub(super) fn stream_all_args(fields: &[(&str, &str)]) -> String {
    let body: Vec<String> = fields
        .iter()
        .map(|(key, value)| {
            format!(
                "{}:{}",
                serde_json::to_string(key).expect("key"),
                serde_json::to_string(value).expect("value")
            )
        })
        .collect();
    format!("{{{}}}", body.join(","))
}

/// The five demo calls: (id, name, serialized arguments). Every call streams
/// for at least STREAM_ALL_MIN_MS_PER_CALL — the demo exists to be WATCHED
pub(super) fn stream_all_calls() -> Vec<(&'static str, &'static str, String)> {
    let old = (7..=14)
        .map(|n| format!("line {n:02} demo"))
        .collect::<Vec<_>>()
        .join("\n");
    let new = std::iter::once("LINE 07 REPLACED".to_string())
        .chain((8..=14).map(|n| format!("changed line {n:02} 修改过的行")))
        .chain([
            "插入的结尾行 inserted tail".to_string(),
            "tail 2".to_string(),
        ])
        .collect::<Vec<_>>()
        .join("\n");
    let command = (1..=13)
        .map(|n| format!("echo demo line {n:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    vec![
        (
            "call_write",
            "Write",
            stream_all_args(&[
                ("path", STREAM_ALL_FILE),
                ("content", &stream_all_write_content()),
            ]),
        ),
        (
            "call_edit",
            "Edit",
            stream_all_args(&[
                ("path", STREAM_ALL_FILE),
                ("old_string", &old),
                ("new_string", &new),
            ]),
        ),
        (
            "call_bash",
            "ExecCommand",
            stream_all_args(&[("command", &command)]),
        ),
        (
            "call_grep",
            "Grep",
            stream_all_args(&[("pattern", "REPLACED"), ("path", STREAM_ALL_FILE)]),
        ),
        (
            "call_read",
            "Read",
            stream_all_args(&[("path", STREAM_ALL_FILE)]),
        ),
    ]
}

/// Split serialized arguments into streaming fragments. With escaped `\n`
/// present: cut after every one (each content line flushes its own live
/// delta — the provider batches on newline) plus after every field boundary
/// (each field lands as its own visible stage: Edit's red old_string rows
/// appear before the green new_string ones). Without newlines (summary-only
/// tools like Grep/Read) the whole JSON drips in equal char pieces so the
/// summary row visibly types out
pub(super) fn stream_all_fragments(arguments: &str) -> Vec<String> {
    const DRIP_PIECES: usize = 8;
    if !arguments.contains("\\n") {
        let chars: Vec<char> = arguments.chars().collect();
        let size = chars.len().div_ceil(DRIP_PIECES);
        return chars
            .chunks(size)
            .map(|piece| piece.iter().collect())
            .collect();
    }
    let mut fragments = Vec::new();
    let mut rest = arguments;
    while !rest.is_empty() {
        let newline = rest.find("\\n").map(|p| p + 2);
        let field = rest.find("\",\"").map(|p| p + 2);
        let Some(cut) = [newline, field].into_iter().flatten().reduce(usize::min) else {
            break;
        };
        fragments.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        fragments.push(rest.to_string());
    }
    fragments
}

/// Per-fragment cadence per tool, chosen so fragments × cadence ≥ 5s of
/// visible streaming per call (Write 42 fragments, Edit 19, Bash 13; the
/// drip tools produce 8)
pub(super) fn stream_all_fragment_ms(tool: &str) -> u64 {
    match tool {
        "Write" => 130,
        "Edit" => 280,
        "ExecCommand" => 420,
        _ => 750,
    }
}

/// Minimum visible streaming time per demo call — asserted by the test
#[cfg(test)]
pub(super) const STREAM_ALL_MIN_MS_PER_CALL: u64 = 5_000;

/// Chat-completions frames for the demo: ONE call per response (tool_results
/// = how many calls have already executed → which call streams now), so each
/// card goes through the full pending → running → settled lifecycle before
/// the next response starts. Per call: a start chunk (id+name, empty
/// arguments — the empty "…" pending card reads for a beat), then one chunk
/// per argument fragment; finish_reason + usage on the last frame
pub(super) fn stream_all_chat_frames(tool_results: usize) -> Vec<(u64, String)> {
    let calls = stream_all_calls();
    let Some((id, name, arguments)) = calls.get(tool_results) else {
        return vec![
            (
                100,
                sse_chunk(
                    serde_json::json!({"content": format!("Streaming demo finished: {STREAM_ALL_MARKER}")}),
                    None,
                ),
            ),
            (50, sse_chunk(serde_json::json!({}), Some("stop"))),
        ];
    };
    let mut out = vec![(
        500,
        sse_chunk(
            serde_json::json!({"tool_calls": [{
                "index": 0, "id": id,
                "function": {"name": name, "arguments": ""},
            }]}),
            None,
        ),
    )];
    for fragment in stream_all_fragments(arguments) {
        out.push((
            stream_all_fragment_ms(name),
            sse_chunk(
                serde_json::json!({"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": fragment},
                }]}),
                None,
            ),
        ));
    }
    out.push((
        200,
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 80, "completion_tokens": 120, "total_tokens": 200},
            })
        ),
    ));
    out
}

/// Anthropic frames for the demo (one call per response): content_block_start
/// starts the pending card, input_json_delta streams the fragments,
/// content_block_stop closes
pub(super) fn stream_all_anthropic_frames(tool_results: usize) -> Vec<(u64, String)> {
    let calls = stream_all_calls();
    if let Some((id, name, arguments)) = calls.get(tool_results) {
        let mut out = vec![
            (
                0,
                a_sse(
                    "message_start",
                    serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
                ),
            ),
            (
                500,
                a_sse(
                    "content_block_start",
                    serde_json::json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": id, "name": name}}),
                ),
            ),
        ];
        for fragment in stream_all_fragments(arguments) {
            out.push((
                stream_all_fragment_ms(name),
                a_sse(
                    "content_block_delta",
                    serde_json::json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": fragment}}),
                ),
            ));
        }
        out.push((
            60,
            a_sse(
                "content_block_stop",
                serde_json::json!({"type": "content_block_stop", "index": 1}),
            ),
        ));
        out.push((
            200,
            a_sse(
                "message_delta",
                serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 120}}),
            ),
        ));
        return out;
    }
    vec![
        (
            0,
            a_sse(
                "message_start",
                serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
            ),
        ),
        (
            100,
            a_sse(
                "content_block_start",
                serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            ),
        ),
        (
            100,
            a_sse(
                "content_block_delta",
                serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": format!("Streaming demo finished: {STREAM_ALL_MARKER}")}}),
            ),
        ),
        (
            50,
            a_sse(
                "content_block_stop",
                serde_json::json!({"type": "content_block_stop", "index": 0}),
            ),
        ),
        (
            50,
            a_sse(
                "message_delta",
                serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 12}}),
            ),
        ),
    ]
}

/// Responses frames for the demo (one call per response): output_item.added
/// starts the pending card, function_call_arguments.delta streams the
/// fragments, done carries the authoritative whole call
pub(super) fn stream_all_responses_frames(tool_results: usize) -> Vec<(u64, String)> {
    let calls = stream_all_calls();
    let mut out = vec![(0, r_ev(responses_created()))];
    let Some((id, name, arguments)) = calls.get(tool_results) else {
        out.push((
            100,
            r_ev(serde_json::json!({
                "type": "response.output_text.delta",
                "delta": format!("Streaming demo finished: {STREAM_ALL_MARKER}"),
            })),
        ));
        out.push((50, r_ev(responses_completed(100, 20))));
        return out;
    };
    out.push((
        500,
        r_ev(serde_json::json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"type": "function_call", "call_id": id, "name": name, "arguments": ""},
        })),
    ));
    for fragment in stream_all_fragments(arguments) {
        out.push((
            stream_all_fragment_ms(name),
            r_ev(serde_json::json!({
                "type": "response.function_call_arguments.delta", "output_index": 0, "delta": fragment,
            })),
        ));
    }
    out.push((
        60,
        r_ev(serde_json::json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"type": "function_call", "call_id": id, "name": name, "arguments": arguments},
        })),
    ));
    out.push((200, r_ev(responses_completed(100, 120))));
    out
}

// ---------------- Anthropic format ----------------

pub(super) fn a_sse(event: &str, mut data: serde_json::Value) -> String {
    if event == "message_start" && data["message"]["id"].is_null() {
        data["message"]["id"] = serde_json::json!("msg-mock");
    }
    format!("event: {event}\ndata: {data}\n\n")
}

pub(super) fn anthropic_tool_call(
    out: &mut Vec<String>,
    index: usize,
    id: &str,
    name: &str,
    arguments: &str,
) {
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

/// Anthropic streaming-args scenario: tool_use block with per-line
/// input_json_delta fragments (index 1 — a thinking block sits at 0)
pub(super) fn anthropic_stream_args(tool_results: usize) -> Vec<String> {
    if tool_results > 0 {
        let mut out = vec![a_sse(
            "message_start",
            serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
        )];
        out.push(a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ));
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": format!("Streamed write finished: {STREAM_ARGS_MARKER}")}}),
        ));
        out.push(a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ));
        out.push(a_sse(
            "message_delta",
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 12}}),
        ));
        return out;
    }
    let fragments = stream_args_fragments();
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
    )];
    out.push(a_sse(
        "content_block_start",
        serde_json::json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "call_stream_args", "name": "Write"}}),
    ));
    for fragment in &fragments {
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": fragment}}),
        ));
    }
    out.push(a_sse(
        "content_block_stop",
        serde_json::json!({"type": "content_block_stop", "index": 1}),
    ));
    out.push(a_sse(
        "message_delta",
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
    ));
    out
}

// ---------------- OpenAI Responses format (/responses) ----------------

/// One SSE frame carrying a Responses event (no `event:` line — the payload's
/// own `type` is the dispatch key, same as the parser)
pub(super) fn r_ev(event: serde_json::Value) -> String {
    format!(
        "data: {event}

"
    )
}

pub(super) fn responses_created() -> serde_json::Value {
    serde_json::json!({"type": "response.created", "response": {"id": "resp-mock"}})
}

pub(super) fn responses_completed(input: u64, output: u64) -> serde_json::Value {
    serde_json::json!({
        "type": "response.completed",
        "response": {"id": "resp-mock", "usage": {
            "input_tokens": input, "output_tokens": output,
            "total_tokens": input + output,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0},
        }},
    })
}

/// Reasoning summary deltas (chunks of 8 chars, same rhythm as the Anthropic mock)
pub(super) fn responses_reasoning(out: &mut Vec<String>) {
    for piece in MOCK_REASONING.chars().collect::<Vec<_>>().chunks(8) {
        let delta: String = piece.iter().collect();
        out.push(r_ev(serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": "rs_mock", "summary_index": 0, "delta": delta,
        })));
    }
}

/// A complete function_call item (arguments arrive whole in the done event —
/// the parser ignores argument deltas, same as codex-rs)
pub(super) fn responses_tool_call(
    out: &mut Vec<String>,
    call_id: &str,
    name: &str,
    arguments: &str,
) {
    out.push(r_ev(serde_json::json!({
        "type": "response.output_item.done",
        "item": {"type": "function_call", "call_id": call_id, "name": name, "arguments": arguments},
    })));
}

/// Responses streaming-args scenario: output_item.added starts the live card,
/// function_call_arguments.delta streams the per-line fragments, done carries
/// the authoritative whole call
pub(super) fn responses_stream_args(tool_results: usize) -> Vec<String> {
    let mut out = vec![r_ev(responses_created())];
    if tool_results > 0 {
        responses_text(
            &mut out,
            &format!("Streamed write finished: {STREAM_ARGS_MARKER}"),
        );
        out.push(r_ev(responses_completed(100, 42)));
        return out;
    }
    let fragments = stream_args_fragments();
    let arguments =
        serde_json::json!({"path": STREAM_ARGS_FILE, "content": STREAM_ARGS_CONTENT}).to_string();
    out.push(r_ev(serde_json::json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"type": "function_call", "call_id": "call_stream_args", "name": "Write", "arguments": ""},
    })));
    for fragment in &fragments {
        out.push(r_ev(serde_json::json!({
            "type": "response.function_call_arguments.delta", "output_index": 0, "delta": fragment,
        })));
    }
    out.push(r_ev(serde_json::json!({
        "type": "response.output_item.done", "output_index": 0,
        "item": {"type": "function_call", "call_id": "call_stream_args", "name": "Write", "arguments": arguments},
    })));
    out.push(r_ev(responses_completed(100, 20)));
    out
}

pub(super) fn responses_text(out: &mut Vec<String>, text: &str) {
    for piece in text.chars().collect::<Vec<_>>().chunks(9) {
        let delta: String = piece.iter().collect();
        out.push(r_ev(serde_json::json!({
            "type": "response.output_text.delta", "delta": delta,
        })));
    }
}

pub(super) fn native_reasoning_responses(tool_results: usize) -> Vec<String> {
    let mut out = vec![r_ev(responses_created())];
    if tool_results > 0 {
        responses_text(&mut out, "native reasoning replay completed");
        out.push(r_ev(responses_completed(100, 12)));
        return out;
    }
    out.push(r_ev(serde_json::json!({
        "type": "response.reasoning_summary_text.delta",
        "item_id": "rs_native", "summary_index": 0, "delta": "native encrypted reasoning",
    })));
    out.push(r_ev(serde_json::json!({
        "type": "response.output_item.done", "output_index": 0,
        "item": {
            "type": "reasoning", "id": "rs_native",
            "summary": [{"type": "summary_text", "text": "native encrypted reasoning"}],
            "content": [{"type": "reasoning_text", "text": "private reasoning content"}],
            "encrypted_content": "encrypted-rs-native",
        },
    })));
    let arguments = format!("{{\"path\": \"{MOCK_FILE_NAME}\"}}");
    out.push(r_ev(serde_json::json!({
        "type": "response.output_item.done", "output_index": 1,
        "item": {"type": "function_call", "call_id": "call_native_read", "name": "Read", "arguments": arguments},
    })));
    out.push(r_ev(responses_completed(100, 20)));
    out
}

pub(super) fn openai_incomplete(reason: &str) -> Vec<String> {
    let arguments = serde_json::json!({
        "path": INCOMPLETE_TOOL_FILE,
        "content": "this must never be written",
    })
    .to_string();
    vec![
        sse_chunk(
            serde_json::json!({"content": "partial answer before abnormal finish"}),
            None,
        ),
        sse_chunk(
            serde_json::json!({"tool_calls": [{
                "index": 0,
                "id": "call_incomplete_write",
                "function": {"name": "Write", "arguments": arguments},
            }]}),
            Some(reason),
        ),
    ]
}

pub(super) fn responses_incomplete(reason: &str) -> Vec<String> {
    let arguments = serde_json::json!({
        "path": INCOMPLETE_TOOL_FILE,
        "content": "this must never be written",
    })
    .to_string();
    vec![
        r_ev(responses_created()),
        r_ev(serde_json::json!({
            "type": "response.output_text.delta", "delta": "partial answer before abnormal finish",
        })),
        r_ev(serde_json::json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_incomplete_write", "name": "Write", "arguments": arguments},
        })),
        r_ev(serde_json::json!({
            "type": "response.incomplete",
            "response": {"id": "resp-incomplete", "incomplete_details": {"reason": reason}},
        })),
    ]
}

pub(super) fn native_reasoning_anthropic(tool_results: usize) -> Vec<String> {
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
    )];
    if tool_results > 0 {
        out.push(a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ));
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "native reasoning replay completed"}}),
        ));
        out.push(a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ));
        out.push(a_sse(
            "message_delta",
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 12}}),
        ));
        return out;
    }
    out.push(a_sse(
        "content_block_start",
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking"}}),
    ));
    out.push(a_sse(
        "content_block_delta",
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "signed native reasoning"}}),
    ));
    out.push(a_sse(
        "content_block_delta",
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-native"}}),
    ));
    out.push(a_sse(
        "content_block_stop",
        serde_json::json!({"type": "content_block_stop", "index": 0}),
    ));
    out.push(a_sse(
        "content_block_start",
        serde_json::json!({"type": "content_block_start", "index": 1, "content_block": {"type": "redacted_thinking", "data": "redacted-native"}}),
    ));
    out.push(a_sse(
        "content_block_stop",
        serde_json::json!({"type": "content_block_stop", "index": 1}),
    ));
    let arguments = format!("{{\"path\": \"{MOCK_FILE_NAME}\"}}");
    anthropic_tool_call(&mut out, 2, "call_native_read", "Read", &arguments);
    out.push(a_sse(
        "message_delta",
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
    ));
    out
}

pub(super) fn anthropic_incomplete(reason: &str) -> Vec<String> {
    let arguments = serde_json::json!({
        "path": INCOMPLETE_TOOL_FILE,
        "content": "this must never be written",
    })
    .to_string();
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {}}),
    )];
    out.push(a_sse(
        "content_block_start",
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
    ));
    out.push(a_sse(
        "content_block_delta",
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial answer before abnormal finish"}}),
    ));
    out.push(a_sse(
        "content_block_stop",
        serde_json::json!({"type": "content_block_stop", "index": 0}),
    ));
    anthropic_tool_call(&mut out, 1, "call_incomplete_write", "Write", &arguments);
    out.push(a_sse(
        "message_delta",
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {}}),
    ));
    out
}

pub(super) fn responses_media_scenario(tool_results: usize) -> Vec<String> {
    let mut out = vec![r_ev(responses_created())];
    if tool_results == 0 {
        responses_tool_call(
            &mut out,
            "call_media_1",
            "ReadMediaFile",
            &serde_json::json!({"path": "pic.png"}).to_string(),
        );
        out.push(r_ev(responses_completed(100, 20)));
    } else {
        responses_text(&mut out, MEDIA_MARKER);
        out.push(r_ev(responses_completed(100, 12)));
    }
    out
}

pub(super) fn responses_parallel_scenario(tool_results: usize) -> Vec<String> {
    let mut out = vec![r_ev(responses_created())];
    if tool_results == 0 {
        let arguments = serde_json::json!({"path": MOCK_FILE_NAME}).to_string();
        for call_id in PARALLEL_IDS_FOR_MOCK {
            responses_tool_call(&mut out, call_id, "Read", &arguments);
        }
        out.push(r_ev(responses_completed(100, 30)));
    } else {
        responses_text(&mut out, PREFIX_PARALLEL_MARKER);
        out.push(r_ev(responses_completed(100, 12)));
    }
    out
}

pub(super) fn responses_web_search_scenario() -> Vec<String> {
    vec![
        r_ev(responses_created()),
        r_ev(serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "web_search_call",
                "id": "ws_1",
                "status": "completed",
                "action": {
                    "type": "search",
                    "query": "pig code",
                    "sources": [{"type": "url", "url": "https://example.com/action", "title": "Action source"}],
                },
            },
        })),
        r_ev(serde_json::json!({
            "type": "response.output_text.annotation.added",
            "output_index": 1,
            "item_id": "msg_1",
            "annotation": {
                "type": "url_citation",
                "url": "https://example.com/cite",
                "title": "Citation",
                "start_index": 0,
                "end_index": 6,
            },
        })),
        r_ev(serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "type": "message",
                "id": "msg_1",
                "content": [{"type": "output_text", "text": RESPONSES_WEB_SEARCH_TEXT}],
            },
        })),
        r_ev(responses_completed(100, 20)),
    ]
}

pub(super) fn responses_chunks(body: &str, tool_results: usize) -> Vec<String> {
    if body.contains(RESPONSES_WEB_SEARCH_TRIGGER) {
        return responses_web_search_scenario();
    }
    if body.contains(CONTEXT_OVERFLOW_KIMI_STREAM_TRIGGER) {
        return vec![r_ev(serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": CONTEXT_OVERFLOW_KIMI_MARKER},
        }))];
    }
    if body.contains(CONTEXT_OVERFLOW_GLM_STREAM_TRIGGER) {
        return vec![r_ev(serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "code": "1261", "message": CONTEXT_OVERFLOW_GLM_MARKER},
        }))];
    }
    if body.contains(CONTEXT_OVERFLOW_STREAM_TRIGGER) {
        return vec![r_ev(serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "code": "context_length_exceeded",
                "message": CONTEXT_OVERFLOW_MARKER,
            },
        }))];
    }
    let mut out = vec![r_ev(responses_created())];
    let user_text = last_user_text(body);
    if user_text.contains(SCENARIO_BACKGROUND_FAILURE_TRIGGER) {
        if tool_answered(body, "call_background_failure") {
            responses_text(&mut out, "Background command exited with code 7.");
        } else {
            responses_tool_call(
                &mut out,
                "call_background_failure",
                "ExecCommand",
                &serde_json::json!({"command": "exit 7", "run_in_background": true}).to_string(),
            );
        }
        out.push(r_ev(responses_completed(100, 20)));
        return out;
    }
    if user_text.contains(SCENARIO_BACKGROUND_TRIGGER) {
        if tool_answered(body, "call_background_task") {
            responses_text(&mut out, BACKGROUND_TASK_OUTPUT);
        } else {
            responses_tool_call(
                &mut out,
                "call_background_task",
                "ExecCommand",
                &serde_json::json!({
                    "command": format!("sleep 0.05; echo {BACKGROUND_TASK_OUTPUT}"),
                    "run_in_background": true,
                })
                .to_string(),
            );
        }
        out.push(r_ev(responses_completed(100, 20)));
        return out;
    }
    if user_text.contains(SCENARIO_TASK_WAIT_TRIGGER) {
        if tool_answered(body, "call_task_wait") {
            responses_text(&mut out, BACKGROUND_TASK_OUTPUT);
        } else if tool_answered(body, "call_task_wait_background") {
            responses_tool_call(
                &mut out,
                "call_task_wait",
                "TaskWait",
                &serde_json::json!({"timeout_ms": 30_000}).to_string(),
            );
        } else {
            responses_tool_call(
                &mut out,
                "call_task_wait_background",
                "ExecCommand",
                &serde_json::json!({
                    "command": format!("sleep 0.05; echo {BACKGROUND_TASK_OUTPUT}"),
                    "run_in_background": true,
                })
                .to_string(),
            );
        }
        out.push(r_ev(responses_completed(100, 20)));
        return out;
    }
    if body.contains(SUBAGENT_RESUMED_PROMPT) {
        responses_text(
            &mut out,
            &format!("Subagent resumed: task done. {SUBAGENT_RESUMED_DONE}"),
        );
        out.push(r_ev(responses_completed(100, 12)));
        return out;
    }
    if body.contains(NATIVE_REASONING_REPLAY_TRIGGER) {
        return native_reasoning_responses(tool_results);
    }
    if body.contains(SCENARIO_MEDIA_TRIGGER) {
        return responses_media_scenario(tool_results);
    }
    if body.contains(PREFIX_PARALLEL_SCENARIO) {
        return responses_parallel_scenario(tool_results);
    }
    if body.contains(TOOL_ONLY_REPLAY_TRIGGER) {
        if tool_results == 0 {
            return vec![
                r_ev(responses_created()),
                r_ev(
                    serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": {
                        "type": "function_call", "call_id": "call_native_read", "name": "Read", "arguments": "{\"path\":\"README.mock.md\"}",
                    }}),
                ),
                r_ev(responses_completed(100, 20)),
            ];
        }
        return vec![
            r_ev(responses_created()),
            r_ev(
                serde_json::json!({"type": "response.output_text.delta", "delta": "tool-only replay completed"}),
            ),
            r_ev(responses_completed(100, 12)),
        ];
    }
    if body.contains(EMPTY_LENGTH_TRIGGER) {
        return vec![
            r_ev(responses_created()),
            r_ev(
                serde_json::json!({"type": "response.incomplete", "response": {
                    "id": "resp-empty-incomplete", "incomplete_details": {"reason": "max_output_tokens"},
                }}),
            ),
        ];
    }
    if body.contains(UNKNOWN_FINISH_TRIGGER) {
        return vec![
            r_ev(responses_created()),
            r_ev(serde_json::json!({
                "type": "response.output_text.delta", "output_index": 0, "delta": UNKNOWN_TERMINAL_TEXT,
            })),
            r_ev(serde_json::json!({
                "type": "response.output_item.done", "output_index": 1,
                "item": {"type": "function_call", "call_id": "call_unknown_finish", "name": "Write",
                    "arguments": "{\"path\":\"unknown-must-not-write.txt\",\"content\":\"x\"}"},
            })),
            r_ev(serde_json::json!({
                "type": "response.incomplete",
                "response": {"id": "resp-unknown", "incomplete_details": {"reason": "banana"}},
            })),
        ];
    }
    if body.contains(MISSING_FINISH_TRIGGER) {
        return vec![
            r_ev(responses_created()),
            r_ev(
                serde_json::json!({"type": "response.output_text.delta", "delta": "unterminated answer"}),
            ),
        ];
    }
    if body.contains(MALFORMED_STREAM_TRIGGER) {
        return vec!["data: {not-json}\n\n".to_string()];
    }
    if body.contains(FINISH_LENGTH_TRIGGER) {
        return responses_incomplete("max_output_tokens");
    }
    if body.contains(FINISH_CONTENT_FILTER_TRIGGER) {
        return responses_incomplete("content_filter");
    }
    if body.contains(STREAM_ARGS_TRIGGER) {
        return responses_stream_args(tool_results);
    }
    if body.contains(SCENARIO_B_TRIGGER) {
        let script: &[(&str, &str, &str)] = &[
            (
                "call_b_write",
                "Write",
                &serde_json::json!({"path": SCENARIO_B_FILE, "content": SCENARIO_B_CONTENT})
                    .to_string(),
            ),
            (
                "call_b_edit",
                "Edit",
                &serde_json::json!({"path": SCENARIO_B_FILE, "old": "line2", "new": "line two"})
                    .to_string(),
            ),
            (
                "call_b_bash",
                "ExecCommand",
                &serde_json::json!({"command": format!("cat {}", SCENARIO_B_FILE)}).to_string(),
            ),
        ];
        if tool_results < script.len() {
            responses_reasoning(&mut out);
            let (_, name, args) = &script[tool_results];
            responses_tool_call(
                &mut out,
                &format!("call_b_{}", name.to_lowercase()),
                name,
                args,
            );
            out.push(r_ev(responses_completed(80, 15)));
            return out;
        }
        responses_text(
            &mut out,
            &format!("Scenario B finished: {SCENARIO_B_MARKER}"),
        );
        out.push(r_ev(responses_completed(100, 42)));
        return out;
    }
    responses_reasoning(&mut out);
    if tool_results == 0 {
        let arguments = format!("{{\"path\": \"{MOCK_FILE_NAME}\"}}");
        responses_tool_call(&mut out, "call_mock_1", "Read", &arguments);
        out.push(r_ev(responses_completed(100, 20)));
    } else {
        let markdown = format!(
            "## File summary

Contents of `{MOCK_FILE_NAME}`:

```text
A known file used by the pig-core mock provider for self-testing.
```

**Conclusion**: {MOCK_REPLY_MARKER}
"
        );
        responses_text(&mut out, &markdown);
        out.push(r_ev(responses_completed(100, 42)));
    }
    out
}

pub(super) fn anthropic_media_scenario(tool_results: usize) -> Vec<String> {
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
    )];
    if tool_results == 0 {
        anthropic_tool_call(
            &mut out,
            0,
            "call_media_1",
            "ReadMediaFile",
            &serde_json::json!({"path": "pic.png"}).to_string(),
        );
    } else {
        out.push(a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ));
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": MEDIA_MARKER}}),
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

pub(super) fn anthropic_parallel_scenario(tool_results: usize) -> Vec<String> {
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
    )];
    if tool_results == 0 {
        let arguments = serde_json::json!({"path": MOCK_FILE_NAME}).to_string();
        for (index, call_id) in PARALLEL_IDS_FOR_MOCK.iter().enumerate() {
            anthropic_tool_call(&mut out, index, call_id, "Read", &arguments);
        }
    } else {
        out.push(a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ));
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": PREFIX_PARALLEL_MARKER}}),
        ));
        out.push(a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ));
    }
    out.push(a_sse(
        "message_delta",
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": if tool_results == 0 { "tool_use" } else { "end_turn" }}, "usage": {"output_tokens": 30}}),
    ));
    out
}

pub(super) fn anthropic_parallel_slow_followup(body: &str, tool_results: usize) -> Vec<String> {
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
    )];
    if tool_results == 0 {
        let user_text = last_user_text(body);
        let url = user_text
            .split_whitespace()
            .last()
            .unwrap_or("http://127.0.0.1:1/unreachable");
        let fetch_arguments = serde_json::json!({"url": url}).to_string();
        for (index, call_id) in ["call_parallel_slow_1", "call_parallel_slow_2"]
            .iter()
            .enumerate()
        {
            anthropic_tool_call(&mut out, index, call_id, "FetchURL", &fetch_arguments);
        }
        let write_arguments = serde_json::json!({
            "path": "cancel-parallel-followup-must-not-exist.txt",
            "content": "must not execute",
        })
        .to_string();
        anthropic_tool_call(
            &mut out,
            2,
            "call_unexecuted_write",
            "Write",
            &write_arguments,
        );
    } else {
        out.push(a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ));
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": PARALLEL_SLOW_MARKER}}),
        ));
        out.push(a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ));
    }
    out.push(a_sse(
        "message_delta",
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": if tool_results == 0 { "tool_use" } else { "end_turn" }}, "usage": {"output_tokens": 30}}),
    ));
    out
}

/// text reply. With SCENARIO_B_TRIGGER it runs the Write→Edit→Bash chain.
/// Anthropic scenario dispatch: no tool_result → Read tool call; otherwise a
/// text reply. With SCENARIO_B_TRIGGER it runs the Write→Edit→Bash chain.
pub(super) fn anthropic_web_search_scenario() -> Vec<String> {
    vec![
        a_sse(
            "message_start",
            serde_json::json!({"type": "message_start", "message": {"id": "msg-mock", "usage": {"input_tokens": 100}}}),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search"}}),
        ),
        a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":\"pig code\"}"}}),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 1, "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_1",
                "content": [{
                    "type": "web_search_result",
                    "url": "https://example.com/action",
                    "title": "Action source",
                    "encrypted_content": "encrypted-result",
                    "page_age": "2026-01-01",
                }],
            }}),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 1}),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text"}}),
        ),
        a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": ANTHROPIC_WEB_SEARCH_TEXT}}),
        ),
        a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 2, "delta": {"type": "citations_delta", "citation": {
                "type": "web_search_result_location",
                "url": "https://example.com/cite",
                "title": "Citation",
                "encrypted_index": "encrypted-index",
                "cited_text": "Native",
            }}}),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 2}),
        ),
        a_sse(
            "message_delta",
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 20}}),
        ),
    ]
}

/// A paused hosted-search turn: thinking/signature, server_tool_use, the
/// native web_search_tool_result, and one text block. `pause_turn` is a
/// successful step boundary that core must continue, not finish.
pub(super) fn anthropic_pause_chunks(step: usize, text: &str) -> Vec<String> {
    let tool_id = format!("srvtoolu_pause_{step}");
    vec![
        a_sse(
            "message_start",
            serde_json::json!({
                "type": "message_start",
                "message": {"id": format!("msg-pause-{step}"), "usage": {"input_tokens": 100}},
            }),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""},
            }),
        ),
        a_sse(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": format!("pause reasoning {step}")},
            }),
        ),
        a_sse(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": format!("sig-pause-{step}")},
            }),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "server_tool_use", "id": tool_id, "name": "web_search"},
            }),
        ),
        a_sse(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "{\"query\":\"pause\"}"},
            }),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 1}),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 2,
                "content_block": {
                    "type": "web_search_tool_result",
                    "tool_use_id": tool_id,
                    "content": [{
                        "type": "web_search_result",
                        "url": ANTHROPIC_PAUSE_SOURCE_URL,
                        "title": "Pause source",
                        "encrypted_content": format!("encrypted-pause-{step}"),
                    }],
                },
            }),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 2}),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 3, "content_block": {"type": "text"}}),
        ),
        a_sse(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 3,
                "delta": {"type": "text_delta", "text": text},
            }),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 3}),
        ),
        a_sse(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "pause_turn"},
                "usage": {"output_tokens": 20},
            }),
        ),
    ]
}

/// A pause with native server blocks but no text/thinking. The extra
/// `snippet` padding makes the block large enough for preserve/preflight tests
/// without inventing user-visible answer text.
pub(super) fn anthropic_server_only_pause_chunks(
    input_tokens: u64,
    output_tokens: u64,
    result_padding: usize,
) -> Vec<String> {
    vec![
        a_sse(
            "message_start",
            serde_json::json!({
                "type": "message_start",
                "message": {"usage": {"input_tokens": input_tokens}},
            }),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "server_tool_use", "id": "srvtoolu_only", "name": "web_search"},
            }),
        ),
        a_sse(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"query\":\"pause\"}"},
            }),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "web_search_tool_result",
                    "tool_use_id": "srvtoolu_only",
                    "content": [{
                        "type": "web_search_result",
                        "url": ANTHROPIC_PAUSE_SOURCE_URL,
                        "title": "Pause source",
                        "encrypted_content": "encrypted-pause-only",
                        "snippet": "x".repeat(result_padding),
                    }],
                },
            }),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 1}),
        ),
        a_sse(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "pause_turn"},
                "usage": {"output_tokens": output_tokens},
            }),
        ),
    ]
}

pub(super) fn anthropic_text_stop_chunks(text: &str, stop_reason: &str) -> Vec<String> {
    vec![
        a_sse(
            "message_start",
            serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ),
        a_sse(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text},
            }),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ),
        a_sse(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason},
                "usage": {"output_tokens": 20},
            }),
        ),
    ]
}

/// A pause that also contains a local `Read` tool call: core must execute it
/// and close the pair before continuing.
pub(super) fn anthropic_pause_tool_chunks() -> Vec<String> {
    vec![
        a_sse(
            "message_start",
            serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ),
        a_sse(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": ANTHROPIC_PAUSE_TOOL_CONTENT},
            }),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ),
        a_sse(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "tool_use", "id": "call_pause_tool", "name": "Read"},
            }),
        ),
        a_sse(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\"README.mock.md\"}"},
            }),
        ),
        a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 1}),
        ),
        a_sse(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "pause_turn"},
                "usage": {"output_tokens": 20},
            }),
        ),
    ]
}

pub(super) fn anthropic_pause_response(body: &str) -> Option<Vec<String>> {
    if body.contains(ANTHROPIC_PAUSE_SERVER_ONLY_TRIGGER) {
        // One server-blocks-only pause, then a normal answer for the
        // continuation and later turns.
        static SERVER_ONLY_COUNTER: AtomicUsize = AtomicUsize::new(0);
        if SERVER_ONLY_COUNTER.fetch_add(1, Ordering::Relaxed) == 0 {
            return Some(anthropic_server_only_pause_chunks(100, 20, 40));
        }
        return Some(anthropic_text_stop_chunks(
            ANTHROPIC_PAUSE_SERVER_ONLY_DONE,
            "end_turn",
        ));
    }
    if body.contains(ANTHROPIC_PAUSE_PRESERVE_TRIGGER) {
        // The paused response's usage is above the trigger in the core test;
        // the large native result makes the newest segment oversized.
        static PRESERVE_COUNTER: AtomicUsize = AtomicUsize::new(0);
        if PRESERVE_COUNTER.fetch_add(1, Ordering::Relaxed) == 0 {
            return Some(anthropic_server_only_pause_chunks(110_000, 10_000, 40_000));
        }
        return Some(anthropic_text_stop_chunks(
            ANTHROPIC_PAUSE_PRESERVE_DONE,
            "end_turn",
        ));
    }
    if body.contains(ANTHROPIC_PAUSE_ALWAYS_TRIGGER) {
        static PAUSE_ALWAYS_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let step = PAUSE_ALWAYS_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        return Some(anthropic_pause_chunks(
            step,
            &format!("{ANTHROPIC_PAUSE_ALWAYS_PREFIX}{step}"),
        ));
    }
    if body.contains(ANTHROPIC_PAUSE_TOOL_TRIGGER) {
        if body.contains("call_pause_tool") {
            return Some(anthropic_text_stop_chunks(
                ANTHROPIC_PAUSE_TOOL_DONE,
                "end_turn",
            ));
        }
        return Some(anthropic_pause_tool_chunks());
    }
    if body.contains(ANTHROPIC_PAUSE_TWICE_TRIGGER) {
        if body.contains(ANTHROPIC_PAUSE_TWICE_CONTENT_2) {
            return Some(anthropic_text_stop_chunks(
                ANTHROPIC_PAUSE_TWICE_DONE,
                "end_turn",
            ));
        }
        if body.contains(ANTHROPIC_PAUSE_TWICE_CONTENT_1) {
            return Some(anthropic_pause_chunks(2, ANTHROPIC_PAUSE_TWICE_CONTENT_2));
        }
        return Some(anthropic_pause_chunks(1, ANTHROPIC_PAUSE_TWICE_CONTENT_1));
    }
    if body.contains(ANTHROPIC_PAUSE_ONCE_TRIGGER) {
        if body.contains(ANTHROPIC_PAUSE_ONCE_CONTENT) {
            return Some(anthropic_text_stop_chunks(
                ANTHROPIC_PAUSE_ONCE_DONE,
                "end_turn",
            ));
        }
        return Some(anthropic_pause_chunks(1, ANTHROPIC_PAUSE_ONCE_CONTENT));
    }
    if body.contains(ANTHROPIC_STOP_SEQUENCE_TRIGGER) {
        return Some(anthropic_text_stop_chunks(
            ANTHROPIC_STOP_SEQUENCE_TEXT,
            "stop_sequence",
        ));
    }
    None
}

pub(super) fn anthropic_chunks(body: &str, tool_results: usize) -> Vec<String> {
    if let Some(chunks) = anthropic_pause_response(body) {
        return chunks;
    }
    if body.contains(SUBAGENT_RESUMED_PROMPT) {
        return anthropic_text_stop_chunks(
            &format!("Subagent resumed: task done. {SUBAGENT_RESUMED_DONE}"),
            "end_turn",
        );
    }
    if body.contains(ANTHROPIC_WEB_SEARCH_TRIGGER) {
        return anthropic_web_search_scenario();
    }
    if body.contains(CONTEXT_OVERFLOW_KIMI_STREAM_TRIGGER) {
        return vec![a_sse(
            "error",
            serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": CONTEXT_OVERFLOW_KIMI_MARKER},
            }),
        )];
    }
    if body.contains(CONTEXT_OVERFLOW_GLM_STREAM_TRIGGER) {
        return vec![a_sse(
            "error",
            serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "code": "1261", "message": CONTEXT_OVERFLOW_GLM_MARKER},
            }),
        )];
    }
    if body.contains(CONTEXT_OVERFLOW_STREAM_TRIGGER) {
        return vec![a_sse(
            "error",
            serde_json::json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "prompt is too long: 9000 tokens > 8192 maximum",
                },
            }),
        )];
    }
    if body.contains(NATIVE_REASONING_REPLAY_TRIGGER) {
        return native_reasoning_anthropic(tool_results);
    }
    if body.contains(SCENARIO_MEDIA_TRIGGER) {
        return anthropic_media_scenario(tool_results);
    }
    if body.contains(PREFIX_PARALLEL_SCENARIO) {
        return anthropic_parallel_scenario(tool_results);
    }
    if body.contains(SCENARIO_PARALLEL_SLOW_FOLLOWUP_TRIGGER) {
        return anthropic_parallel_slow_followup(body, tool_results);
    }
    if body.contains(UNKNOWN_STOP_TRIGGER) {
        let mut out = vec![a_sse(
            "message_start",
            serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}),
        )];
        out.push(a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ));
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": UNKNOWN_TERMINAL_TEXT}}),
        ));
        out.push(a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ));
        anthropic_tool_call(
            &mut out,
            1,
            "call_unknown_stop",
            "Write",
            &serde_json::json!({"path": "unknown-must-not-write.txt", "content": "x"}).to_string(),
        );
        out.push(a_sse(
            "message_delta",
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "banana"}, "usage": {"output_tokens": 5}}),
        ));
        return out;
    }
    if body.contains(MISSING_FINISH_TRIGGER) {
        return vec![
            a_sse(
                "message_start",
                serde_json::json!({"type": "message_start", "message": {}}),
            ),
            a_sse(
                "content_block_start",
                serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            ),
            a_sse(
                "content_block_delta",
                serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "unterminated answer"}}),
            ),
        ];
    }
    if body.contains(MALFORMED_STREAM_TRIGGER) {
        return vec!["data: {not-json}\n\n".to_string()];
    }
    if body.contains(FINISH_LENGTH_TRIGGER) {
        return anthropic_incomplete("max_tokens");
    }
    if body.contains(FINISH_CONTENT_FILTER_TRIGGER) {
        return anthropic_incomplete("refusal");
    }
    let user_text = last_user_text(body);
    if user_text.contains(SCENARIO_BACKGROUND_FAILURE_TRIGGER) {
        if tool_answered(body, "call_background_failure") {
            return anthropic_text_stop_chunks(
                "Background command exited with code 7.",
                "end_turn",
            );
        }
        let mut out = vec![a_sse(
            "message_start",
            serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
        )];
        anthropic_tool_call(
            &mut out,
            1,
            "call_background_failure",
            "ExecCommand",
            &serde_json::json!({"command": "exit 7", "run_in_background": true}).to_string(),
        );
        out.push(a_sse(
            "message_delta",
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
        ));
        return out;
    }
    if user_text.contains(SCENARIO_BACKGROUND_TRIGGER) {
        if tool_answered(body, "call_background_task") {
            return anthropic_text_stop_chunks(BACKGROUND_TASK_OUTPUT, "end_turn");
        }
        let mut out = vec![a_sse(
            "message_start",
            serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
        )];
        anthropic_tool_call(
            &mut out,
            1,
            "call_background_task",
            "ExecCommand",
            &serde_json::json!({
                "command": format!("sleep 0.05; echo {BACKGROUND_TASK_OUTPUT}"),
                "run_in_background": true,
            })
            .to_string(),
        );
        out.push(a_sse(
            "message_delta",
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
        ));
        return out;
    }
    if user_text.contains(SCENARIO_TASK_WAIT_TRIGGER) {
        if tool_answered(body, "call_task_wait") {
            return anthropic_text_stop_chunks(BACKGROUND_TASK_OUTPUT, "end_turn");
        }
        let mut out = vec![a_sse(
            "message_start",
            serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
        )];
        if tool_answered(body, "call_task_wait_background") {
            anthropic_tool_call(
                &mut out,
                1,
                "call_task_wait",
                "TaskWait",
                &serde_json::json!({"timeout_ms": 30_000}).to_string(),
            );
        } else {
            anthropic_tool_call(
                &mut out,
                1,
                "call_task_wait_background",
                "ExecCommand",
                &serde_json::json!({
                    "command": format!("sleep 0.05; echo {BACKGROUND_TASK_OUTPUT}"),
                    "run_in_background": true,
                })
                .to_string(),
            );
        }
        out.push(a_sse(
            "message_delta",
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
        ));
        return out;
    }
    if body.contains(SCENARIO_B_TRIGGER) {
        return anthropic_scenario_b(tool_results);
    }
    if body.contains(SCENARIO_SLOW_TRIGGER) {
        return anthropic_slow_scenario(tool_results);
    }
    if body.contains(SCENARIO_Q_TRIGGER) {
        return anthropic_question_scenario(tool_results);
    }
    if body.contains(STREAM_ARGS_TRIGGER) {
        return anthropic_stream_args(tool_results);
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

pub(super) fn anthropic_scenario_b(tool_results: usize) -> Vec<String> {
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
    )];
    match tool_results {
        0 => anthropic_tool_call(&mut out, 0, "call_b_write", "Write",
            &serde_json::json!({"path": SCENARIO_B_FILE, "content": SCENARIO_B_CONTENT}).to_string()),
        1 => anthropic_tool_call(&mut out, 0, "call_b_edit", "Edit",
            &serde_json::json!({"path": SCENARIO_B_FILE, "old_string": "line2", "new_string": "LINE2"}).to_string()),
        2 => anthropic_tool_call(&mut out, 0, "call_b_bash", "ExecCommand",
            // Same rationale as the OpenAI branch: not in the read-only allowlist (echo is)
            &serde_json::json!({"command": scenario_b_bash_command()}).to_string()),
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

/// Anthropic slow-command scenario (interrupt tests): no tool_result → a
/// 30-second ExecCommand call, otherwise text containing the slow marker.
pub(super) fn anthropic_slow_scenario(tool_results: usize) -> Vec<String> {
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
    )];
    if tool_results == 0 {
        anthropic_tool_call(
            &mut out,
            0,
            "call_slow_bash",
            "ExecCommand",
            &serde_json::json!({"command": "sleep 30"}).to_string(),
        );
    } else {
        out.push(a_sse(
            "content_block_start",
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ));
        out.push(a_sse(
            "content_block_delta",
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": SLOW_MARKER}}),
        ));
        out.push(a_sse(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ));
    }
    out.push(a_sse(
        "message_delta",
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": if tool_results == 0 { "tool_use" } else { "end_turn" }}, "usage": {"output_tokens": 42}}),
    ));
    out
}

/// Anthropic AskUserQuestion scenario: no tool_result → the question tool call; otherwise text containing the marker.
pub(super) fn anthropic_question_scenario(tool_results: usize) -> Vec<String> {
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

#[cfg(test)]
mod stream_all_tests {
    use super::*;

    /// The demo splitter cuts after every escaped newline (each content line
    /// flushes its own provider delta) and after every field boundary (Edit's
    /// old_string lands before new_string); the fragments reassemble to the
    /// exact original arguments
    #[test]
    fn stream_all_fragments_cut_and_reassemble() {
        let arguments = stream_all_args(&[
            ("path", "a.txt"),
            ("old_string", "l1\nl2\n"),
            ("new_string", "x"),
        ]);
        let fragments = stream_all_fragments(&arguments);
        assert_eq!(fragments.concat(), arguments);
        // Field boundary first: the path fragment lands before any content
        assert_eq!(fragments[0], r#"{"path":"a.txt","#);
        // Every content line ends its own fragment (the provider's newline
        // batching flushes it as a live delta)
        let line_fragments: Vec<_> = fragments.iter().filter(|f| f.ends_with("\\n")).collect();
        assert_eq!(line_fragments.len(), 2);
        // The demo calls: Write fragments carry one content line each (40
        // content lines + the path fragment + the closing fragment)
        let calls = stream_all_calls();
        let write = stream_all_fragments(&calls[0].2);
        assert_eq!(write.concat(), calls[0].2);
        assert_eq!(write.len(), 40 + 2);
        // The Write path streams first (the pending card shows the file chip
        // from the first fragment, then the content grows)
        assert!(write[0].contains("\"path\""), "path must stream first");
        // The Edit fragments order old_string before new_string (red rows
        // before green rows in the streaming box)
        let edit = stream_all_fragments(&calls[1].2);
        let joined: String = edit.concat();
        assert!(
            joined.find("old_string").unwrap() < joined.find("new_string").unwrap(),
            "old_string must stream before new_string"
        );
        // Newline-less arguments (Grep/Read) drip in equal char pieces so the
        // summary row visibly types out
        let grep = stream_all_fragments(&calls[3].2);
        assert_eq!(grep.concat(), calls[3].2);
        assert_eq!(grep.len(), 8, "the drip produces 8 pieces");
    }

    /// The demo exists to be WATCHED: every call streams for at least
    /// STREAM_ALL_MIN_MS_PER_CALL (fragment count × per-tool cadence)
    #[test]
    fn stream_all_each_call_streams_at_least_five_seconds() {
        for (id, name, arguments) in stream_all_calls() {
            let fragments = stream_all_fragments(&arguments);
            let ms = fragments.len() as u64 * stream_all_fragment_ms(name);
            assert!(
                ms >= STREAM_ALL_MIN_MS_PER_CALL,
                "{id} ({name}): {} fragments × {}ms = {ms}ms < {STREAM_ALL_MIN_MS_PER_CALL}ms",
                fragments.len(),
                stream_all_fragment_ms(name),
            );
        }
    }

    /// Every format streams one call per response (tool_results selects it),
    /// so each card settles before the next streams; the final reply carries
    /// the marker
    #[test]
    fn stream_all_frames_cover_all_formats() {
        let calls = stream_all_calls();
        for (ix, (_, name, _)) in calls.iter().enumerate() {
            let chat: String = stream_all_chat_frames(ix)
                .iter()
                .map(|(_, f)| f.as_str())
                .collect();
            assert!(chat.contains(name), "chat frames at {ix} miss {name}");
            // One call per response: the NEXT call's name must not appear yet
            if let Some((_, next, _)) = calls.get(ix + 1) {
                assert!(
                    !chat.contains(next),
                    "chat frames at {ix} already carry {next}"
                );
            }
            assert!(stream_all_anthropic_frames(ix).len() > 3);
            assert!(stream_all_responses_frames(ix).len() > 3);
        }
        let done = calls.len();
        assert!(
            stream_all_chat_frames(done)[0]
                .1
                .contains(STREAM_ALL_MARKER)
        );
        assert!(
            stream_all_anthropic_frames(done)
                .iter()
                .any(|(_, f)| f.contains(STREAM_ALL_MARKER))
        );
        assert!(
            stream_all_responses_frames(done)
                .iter()
                .any(|(_, f)| f.contains(STREAM_ALL_MARKER))
        );
    }
}
