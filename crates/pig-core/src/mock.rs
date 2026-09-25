//! 模拟 OpenAI Chat Completions + SSE 的 provider，行为对齐 GLM/DeepSeek：
//! 首请求返回 Read 工具调用（arguments 分片），含工具结果后返回
//! reasoning_content + Markdown 文本流。供集成测试、examples 与 GUI 自测复用。

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use std::sync::atomic::{AtomicUsize, Ordering};

pub const MOCK_FILE_NAME: &str = "README.mock.md";
pub const MOCK_FILE_CONTENT: &str =
    "# mock 文件\n\n这是 pig-core mock provider 自测用的已知文件。\n";
pub const MOCK_REASONING: &str = "用户让我读一个文件并总结。先调用 Read。";
pub const MOCK_REPLY_MARKER: &str = "MOCK_REPLY_OK";

/// 场景 B：用户消息含此标记时，走 Write → Edit → Bash → 文本 的完整修改链。
pub const SCENARIO_B_TRIGGER: &str = "SCENARIO_B";
pub const SCENARIO_B_MARKER: &str = "MOCK_SCENARIO_B_OK";
pub const SCENARIO_B_FILE: &str = "src/hello.txt";
pub const SCENARIO_B_CONTENT: &str = "hello\nline2\nline3\n";
pub const SCENARIO_B_BASH_MARKER: &str = "MOCK_BASH_OK";

/// TodoList 场景：含此标记时走 TodoList 写入 → 文本（测待办持久化用）。
pub const TODO_SCENARIO_TRIGGER: &str = "TODO_SCENARIO";
pub const TODO_SCENARIO_MARKER: &str = "MOCK_TODO_OK";
pub const TODO_SCENARIO_ITEM: &str = "持久化待办项";

/// AskUserQuestion 场景：含此标记且无工具结果 → AskUserQuestion 调用（1 题 2 选项）；
/// 有工具结果 → 文本含 marker。
pub const SCENARIO_Q_TRIGGER: &str = "SCENARIO_Q";
pub const MOCK_Q_MARKER: &str = "MOCK_QUESTION_OK";

/// 危险命令场景：含此标记时按 tool 结果数推进——0/1 个结果都发危险 Bash（第二条
/// 用于验证 AlwaysAllow 对危险命令不记忆），≥2 → 文本含 marker。
/// 命令选 mkfs：命中黑名单（mkfs 命令位）但执行无害——macOS 无此命令（exit 127），
/// Linux 无参数调用只打印用法，不触碰磁盘。
pub const SCENARIO_DANGER_TRIGGER: &str = "DANGER_SCENARIO";
pub const DANGER_COMMAND: &str = "mkfs";
pub const DANGER_MARKER: &str = "MOCK_DANGER_DONE";

/// subject 粒度场景（AlwaysAllow 细化验证）：Write a → Write a → Write b →
/// Bash echo → Bash echo → Bash ls → 文本。同 subject 的第二次不应再弹审批。
pub const SCENARIO_SUBJECT_TRIGGER: &str = "SUBJECT_SCENARIO";
pub const SUBJECT_MARKER: &str = "MOCK_SUBJECT_DONE";
pub const SUBJECT_FILE_A: &str = "subject_a.txt";
pub const SUBJECT_FILE_B: &str = "subject_b.txt";

/// 只读命令场景（AutoEdit 直通验证）：Bash ls → 文本。ls 在只读白名单内。
pub const SCENARIO_READONLY_TRIGGER: &str = "READONLY_SCENARIO";
pub const READONLY_MARKER: &str = "MOCK_READONLY_DONE";

/// 计划退出场景（ExitPlanMode 验证）：0 个结果 → ExitPlanMode；
/// 1 个结果且含「已切换到」（用户 Allow）→ Write plan_exit.txt；否则文本收尾
///（Reject 的结果不含切换文案 → 直接收尾）。
pub const SCENARIO_PLAN_EXIT_TRIGGER: &str = "PLAN_EXIT_SCENARIO";
pub const PLAN_EXIT_MARKER: &str = "MOCK_PLAN_EXIT_DONE";
pub const PLAN_EXIT_FILE: &str = "plan_exit.txt";

/// 计划进入/恢复场景（EnterPlanMode 验证）：EnterPlanMode → Write（应被 Plan
/// 硬拒）→ ExitPlanMode → Write（恢复原模式后执行）→ 文本。
pub const SCENARIO_PLAN_ENTER_TRIGGER: &str = "PLAN_ENTER_SCENARIO";
pub const PLAN_ENTER_MARKER: &str = "MOCK_PLAN_ENTER_DONE";
pub const PLAN_ENTER_FILE: &str = "plan_enter.txt";

/// 图片工具场景（ReadMediaFile 验证）：0 个结果 → ReadMediaFile pic.png → 文本。
/// 能力门控（input_image=false 时会话层直接报错不执行）与图片进上下文链路用。
pub const SCENARIO_MEDIA_TRIGGER: &str = "MEDIA_SCENARIO";
pub const MEDIA_MARKER: &str = "MOCK_MEDIA_DONE";

/// 起一个独立线程运行 tokio runtime 服务 mock SSE，返回监听端口。
pub fn start_mock_server() -> u16 {
    start_mock_server_with_log().0
}

/// 同上，但额外返回请求体日志（测试断言 reasoning_params merge 等用）。
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

/// usage: Some((prompt, completion, total)) 时并入带 finish_reason 的末块
///（provider 见到 finish_reason 即收尾，usage 必须同块到达，对齐真实 API）
fn tool_call_chunks(
    call_id: &str,
    name: &str,
    arguments: &str,
    usage: Option<(u64, u64, u64)>,
) -> Vec<String> {
    // 分片点在字符边界上取（中文参数被切到多字节字符中间会 panic）
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
    // 中间 step 也上报用量（真实 API 每次请求都带）：水位应以最后一步的 142 为准，
    // 回合累计（turn_stats）则是各步之和
    chunks.extend(tool_call_chunks(
        "call_mock_1",
        "Read",
        &arguments,
        Some((60, 10, 70)),
    ));
    chunks
}

/// 回显消息数（测试 resume 后历史重建用）：用户消息含 ECHO_HISTORY 时触发。
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

/// 回显 system prompt（测试 AGENTS.md 注入用）：用户消息含 ECHO_SYSTEM 时触发。
fn echo_system_response(body: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let system = parsed["messages"]
        .as_array()
        .and_then(|msgs| msgs.iter().find(|m| m["role"].as_str() == Some("system")))
        .and_then(|m| m["content"].as_str().map(str::to_string))
        .unwrap_or_else(|| "(no system message)".to_string());
    let system: String = system.chars().take(3000).collect();
    let chars: Vec<char> = system.chars().collect();
    let mut chunks: Vec<String> = chars
        .chunks(40)
        .map(|piece| {
            let delta: String = piece.iter().collect();
            sse_chunk(serde_json::json!({"content": delta}), None)
        })
        .collect();
    chunks.push(sse_chunk(serde_json::json!({}), Some("stop")));
    chunks
}

/// 场景 B 按历史里 tool 结果的数量推进：0→Write，1→Edit，2→Bash，≥3→文本。
/// file 参数化避免多个自测会话改同一文件互相干扰。
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
            // printf 不在只读白名单（echo 在）：AutoEdit 审批语义靠这条命令覆盖
            &serde_json::json!({"command": format!("printf '%s\\n' {SCENARIO_B_BASH_MARKER}")})
                .to_string(),
            None,
        ),
        _ => {
            let markdown = format!(
                "已完成：创建 `{SCENARIO_B_FILE}`、修改一行、执行 echo。\n\n**结果**: {SCENARIO_B_MARKER}\n"
            );
            let chars: Vec<char> = markdown.chars().collect();
            let mut chunks: Vec<String> = chars
                .chunks(9)
                .map(|piece| {
                    let delta: String = piece.iter().collect();
                    sse_chunk(serde_json::json!({"content": delta}), None)
                })
                .collect();
            // 终步带 usage（真实 API 流末带用量；水位条/回合统计靠它）
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

/// 危险命令场景：0/1 个 tool 结果都发危险 Bash，≥2 → 文本收尾。
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

/// subject 粒度场景：同 subject 第二次不弹（Write a 连写两次）、异 subject 弹。
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

/// 只读命令场景：Bash ls → 文本。
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

/// 计划退出场景：按 tool 结果数推进；Allow 后（结果含「已切换到」）接 Write。
fn plan_exit_scenario_response(body: &str, tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_pe_1",
            "ExitPlanMode",
            &serde_json::json!({"plan": "第一步：创建 plan_exit.txt 验证执行"}).to_string(),
            None,
        ),
        1 if body.contains("已切换到") => tool_call_chunks(
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

/// 计划进入/恢复场景：纯按 tool 结果数推进（Write 被 Plan 硬拒也算一步）。
fn plan_enter_scenario_response(tool_results: usize) -> Vec<String> {
    match tool_results {
        0 => tool_call_chunks(
            "call_pn_1",
            "EnterPlanMode",
            &serde_json::json!({"reason": "改动范围大，先出计划"}).to_string(),
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
            &serde_json::json!({"plan": "计划就绪"}).to_string(),
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

/// 图片工具场景：ReadMediaFile pic.png → 文本。
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

/// TodoList 场景：历史里还没有 TodoList 调用 → 写入；已执行 → 文本收尾。
/// 不能按全局 tool 结果计数：请求体的 tools 声明与历史消息都会干扰，
/// 直接解析 messages 里是否出现过 TodoList 调用。
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
                {"content": format!("{TODO_SCENARIO_ITEM}一"), "status": "done"},
                {"content": format!("{TODO_SCENARIO_ITEM}二"), "status": "in_progress"}
            ]})
            .to_string(),
            None,
        )
    }
}

/// AskUserQuestion 场景：历史里还没有 AskUserQuestion 调用 → 提问；已执行 → 文本收尾。
///（同 TodoList 场景：直接扫 messages，不能按全局 tool 结果计数）
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
                    "question": "选择实现方案",
                    "header": "方案",
                    "options": [
                        {"label": "方案 A", "description": "简单直接"},
                        {"label": "方案 B", "description": "更完善但复杂"}
                    ]
                },
                {
                    "question": "需要跑测试吗",
                    "header": "测试",
                    "options": [
                        {"label": "要", "description": "改完跑一遍"},
                        {"label": "不要", "description": "先不跑"}
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
        "## 文件摘要\n\n`{MOCK_FILE_NAME}` 的内容如下：\n\n```text\n这是 pig-core mock provider 自测用的已知文件。\n```\n\n**结论**: {MOCK_REPLY_MARKER}\n"
    );
    let mut chunks = vec![sse_chunk(
        serde_json::json!({"reasoning_content": "工具已返回文件内容，组织回答。"}),
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
pub const PLAN_CONFIRM_TEXT: &str = "计划已确认";
/// 会话自动命名 sidecar 的 mock 标题（selftest 断言用）
pub const MOCK_TITLE: &str = "自动命名自测标题";

/// 非流式的自动命名响应：{"title": MOCK_TITLE}（两种 API 格式同内容）。
/// 请求识别见 session::TITLE_PROMPT_MARKER
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

/// 非流式响应（compact 摘要）。FAIL_COMPACT 触发 500 测试回退路径。
async fn write_json_response(stream: &mut tokio::net::TcpStream, body: &str) -> bool {
    if body.contains("FAIL_COMPACT") {
        let resp = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n\r\n{}";
        return stream.write_all(resp.as_bytes()).await.is_ok();
    }
    let content = format!("{SUMMARY_MARKER}：用户目标=mock 自测；已完成=读取/写入文件；待办=无。");
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

/// 场景 C：计划模式，直接输出 markdown 计划（无工具调用）。
fn scenario_c_response() -> Vec<String> {
    let plan = format!(
        "## 执行计划

1. 创建 `src/hello.txt` 写入三行内容
2. 将第二行改为大写
3. 运行 echo 验证

**计划就绪**: {PLAN_MARKER}
"
    );
    let chars: Vec<char> = plan.chars().collect();
    let mut chunks: Vec<String> = chars
        .chunks(9)
        .map(|piece| {
            let delta: String = piece.iter().collect();
            sse_chunk(serde_json::json!({"content": delta}), None)
        })
        .collect();
    chunks.push(sse_chunk(serde_json::json!({}), Some("stop")));
    chunks
}

/// ECHO_USAGE <n>：文本回复 + 指定 total_tokens（测试自动 compact 触发）。
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

    // 重试测试：首个含 FAIL_ONCE_500 的请求返回 500（计数耗尽后正常）
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

    // 非流式 = 自动命名 / compact 摘要 / 连通性测试（必须在 SSE 响应头之前分支）
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
    let chunks = if anthropic {
        anthropic_chunks(&body, tool_results)
    } else if body.contains("ECHO_SYSTEM") {
        echo_system_response(&body)
    } else if body.contains("ECHO_USAGE") {
        echo_usage_response(&body)
    } else if body.contains("ECHO_HISTORY") {
        echo_history_response(&body)
    } else if body.contains(PLAN_CONFIRM_TEXT) && body.contains(SCENARIO_C_TRIGGER) {
        scenario_b_response(tool_results, "src/hello_plan.txt")
    } else if body.contains(SCENARIO_C_TRIGGER) {
        scenario_c_response()
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
    } else if body.contains(SCENARIO_PLAN_EXIT_TRIGGER) {
        plan_exit_scenario_response(&body, tool_results)
    } else if body.contains(SCENARIO_PLAN_ENTER_TRIGGER) {
        plan_enter_scenario_response(tool_results)
    } else if body.contains(SCENARIO_MEDIA_TRIGGER) {
        media_scenario_response(tool_results)
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

// ---------------- Anthropic 格式 ----------------

fn a_sse(event: &str, data: serde_json::Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn anthropic_tool_call(out: &mut Vec<String>, index: usize, id: &str, name: &str, arguments: &str) {
    out.push(a_sse(
        "content_block_start",
        serde_json::json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": id, "name": name}}),
    ));
    // 分片点在字符边界上取（中文参数被切到多字节字符中间会 panic）
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

/// Anthropic 版场景分发：无 tool_result → Read 工具调用；否则文本回复。
/// 含 SCENARIO_B_TRIGGER 时走 Write→Edit→Bash 链。
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

    // 思考块
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
        // Read 工具调用，arguments 分片（分片点取字符边界）
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
            "## 文件摘要\n\n`{MOCK_FILE_NAME}` 的内容如下：\n\n```text\n这是 pig-core mock provider 自测用的已知文件。\n```\n\n**结论**: {MOCK_REPLY_MARKER}\n"
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
            // 与 OpenAI 分支同口径：printf 不在只读白名单（echo 在）
            &serde_json::json!({"command": format!("printf '%s\\n' {SCENARIO_B_BASH_MARKER}")}).to_string()),
        _ => {
            let text = format!("场景B完成。**结果**: {SCENARIO_B_MARKER}
");
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

/// Anthropic 版 AskUserQuestion 场景：无 tool_result → 提问工具调用；否则文本含 marker。
fn anthropic_question_scenario(tool_results: usize) -> Vec<String> {
    let mut out = vec![a_sse(
        "message_start",
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 100}}}),
    )];
    if tool_results == 0 {
        let arguments = serde_json::json!({"questions": [
            {
                "question": "选择实现方案",
                "header": "方案",
                "options": [
                    {"label": "方案 A", "description": "简单直接"},
                    {"label": "方案 B", "description": "更完善但复杂"}
                ]
            },
            {
                "question": "需要跑测试吗",
                "header": "测试",
                "options": [
                    {"label": "要", "description": "改完跑一遍"},
                    {"label": "不要", "description": "先不跑"}
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
    let content = format!("{SUMMARY_MARKER}：用户目标=mock 自测；已完成=读取/写入文件；待办=无。");
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
