//! 会话内上下文字节稳定性（缓存前缀契约）：
//! 两个真实回合之间修改 AGENTS.md / 技能目录 / 子代理档案，发给模型的
//! tools 与 system（乃至整个旧消息前缀）必须逐字节不变——变化只允许出现在
//! 尾部新增的消息上（reminder 骑在最新用户消息前）。
//! 这是「冻结 + reminder」架构的回归防线：谁把重扫引回热路径，这里先红。

mod common;

use pig_core::mock;
use pig_core::spawn_agent_with_data_dir;
use pig_protocol::{Event, Op};
use std::time::Duration;

/// 主循环请求 = messages[0] 是 pig-code 系统 prompt 且带 tools 数组
/// （标题生成等旁路请求会被日志一并记录，需过滤）
fn main_loop_requests(log: &[String]) -> Vec<serde_json::Value> {
    log.iter()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .filter(|req| {
            req.get("tools").is_some_and(|t| t.as_array().is_some_and(|a| !a.is_empty()))
                && req["messages"][0]["role"] == "system"
                && req["messages"][0]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("pig-code"))
        })
        .collect()
}

fn messages_of(req: &serde_json::Value) -> &Vec<serde_json::Value> {
    req["messages"].as_array().expect("messages 数组")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_prefix_stable_across_turns_and_env_changes() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-ctx-stable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ws = dir.join("ws");
    let data = dir.join("data");
    std::fs::create_dir_all(ws.join(".pigcode").join("agents")).unwrap();
    std::fs::create_dir_all(data.join("skills").join("demo")).unwrap();
    // 会话开始前就存在的环境：AGENTS.md / 技能 / 自定义子代理档案
    std::fs::write(ws.join("AGENTS.md"), "原始 AGENTS 规则").unwrap();
    std::fs::write(
        data.join("skills").join("demo").join("SKILL.md"),
        "---\nname: demo\ndescription: 演示技能\n---\n技能正文",
    )
    .unwrap();
    std::fs::write(
        ws.join(".pigcode").join("agents").join("custom.md"),
        "---\nname: custom\ndescription: 自定义档案\n---\n档案正文。",
    )
    .unwrap();
    std::fs::write(ws.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock 供应商"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
"#
        ),
    )
    .unwrap();

    let agent = spawn_agent_with_data_dir(Some(config_path), ws.clone(), data.clone());
    let session_id = common::new_session(&agent, ws.clone()).await;

    // ---- 回合 1 ----
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "读取 README.mock.md 并总结".to_string(),
            files: vec![],
            images: vec![],
            mode: pig_protocol::ExecMode::ConfirmBeforeEdit,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let requests = main_loop_requests(&log.lock().unwrap());
    assert!(!requests.is_empty(), "回合 1 应有主循环请求");
    let turn1 = requests.last().expect("回合 1 最后一个请求").clone();
    let turn1_messages = messages_of(&turn1);
    let turn1_len = turn1_messages.len();
    // 系统 prompt 应含冻结的环境段；首条用户消息带模式 reminder
    assert!(turn1_messages[0]["content"].as_str().unwrap().contains("原始 AGENTS 规则"));
    assert!(turn1_messages[0]["content"].as_str().unwrap().contains("- demo: 演示技能"));
    assert!(
        turn1_messages[1]["content"].as_str().unwrap().contains("当前执行模式"),
        "回合 1 用户消息前应有模式 reminder"
    );

    // ---- 会话中途改环境：AGENTS.md 换内容、新增技能、新增子代理档案 ----
    std::fs::write(ws.join("AGENTS.md"), "全新的 AGENTS 规则").unwrap();
    std::fs::create_dir_all(data.join("skills").join("second")).unwrap();
    std::fs::write(
        data.join("skills").join("second").join("SKILL.md"),
        "---\nname: second\ndescription: 新技能\n---\n正文",
    )
    .unwrap();
    std::fs::write(
        ws.join(".pigcode").join("agents").join("more.md"),
        "---\nname: more\ndescription: 中途新增档案\n---\n档案正文。",
    )
    .unwrap();

    // ---- 回合 2 ----
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "继续".to_string(),
            files: vec![],
            images: vec![],
            mode: pig_protocol::ExecMode::ConfirmBeforeEdit,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let requests = main_loop_requests(&log.lock().unwrap());
    let turn2 = requests.last().expect("回合 2 最后一个请求").clone();
    let turn2_messages = messages_of(&turn2);

    // ---- 缓存前缀契约：system、tools、全部旧消息逐字节不变 ----
    assert_eq!(
        turn1_messages[0], turn2_messages[0],
        "系统提示词跨回合+环境变更必须字节稳定"
    );
    assert_eq!(
        turn1["tools"], turn2["tools"],
        "tools 数组跨回合+环境变更（含中途新增子代理档案/技能）必须字节稳定"
    );
    assert!(
        turn2_messages.len() >= turn1_len,
        "回合 2 只应在尾部追加消息，不该改写旧消息"
    );
    for (i, msg) in turn1_messages.iter().enumerate() {
        assert_eq!(
            msg,
            &turn2_messages[i],
            "旧消息[{i}]被改写：缓存前缀从该处失效"
        );
    }

    // ---- 变化只出现在尾部：新用户消息 = reminder（模式 + AGENTS.md 变更推送）+ 原文 ----
    // （回合 1 的收尾 assistant 文本也在前缀里，位置索引会漂移，按内容定位）
    let new_user = turn2_messages
        .iter()
        .rev()
        .find(|m| {
            m["role"] == "user" && m["content"].as_str().is_some_and(|c| c.ends_with("继续"))
        })
        .and_then(|m| m["content"].as_str())
        .expect("回合 2 的新用户消息");
    assert!(
        new_user.starts_with("<system-reminder>"),
        "reminder 应 prepend 到新用户消息，实际: {new_user}"
    );
    assert!(new_user.contains("当前执行模式"));
    assert!(new_user.contains("AGENTS.md 内容有更新"));
    assert!(new_user.contains("全新的 AGENTS 规则"), "reminder 应携带最新 AGENTS.md 内容");
    assert!(new_user.ends_with("继续"), "原文保持在 reminder 之后");
    // 新技能/新档案不进冻结段（对 system 的断言已隐含），也不进 tools（上面已断言）

    let _ = std::fs::remove_dir_all(&dir);
}
