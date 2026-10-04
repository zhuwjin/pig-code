mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

/// 主会话每个 provider 步骤落盘一条调用轨迹（{session}.model-io.jsonl）：
/// 发一轮消息后应至少有两条记录（含工具调用步 + 收尾步），字段齐且输入
/// 含用户消息投影
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_io_written_per_step() {
    let (config_path, cwd, data_dir) = setup("model-io");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: "读一下 mock 文件并总结".into(),
            files: vec![mock::MOCK_FILE_NAME.into()],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    let _ = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    let path = pig_core::model_io::model_io_path(&data_dir.join("sessions"), &sid);
    assert!(path.exists(), "轨迹文件应已落盘: {}", path.display());
    let records = pig_core::model_io::read_all(&path);
    assert!(
        records.len() >= 2,
        "一轮含工具调用的对话至少两步，实际 {} 条",
        records.len()
    );
    for record in &records {
        assert_eq!(record.source, "main");
        assert_eq!(record.provider, "Mock 供应商");
        assert_eq!(record.model, "mock-model");
        assert!(
            record.turn.starts_with('t'),
            "turn 标识形如 {{turn}}-s{{step}}"
        );
        assert!(
            matches!(record.finish.as_str(), "stop" | "tool_calls"),
            "结束原因应合法: {}",
            record.finish
        );
        assert!(!record.input.is_empty(), "输入投影不应为空");
        assert!(record.duration_ms > 0 || record.usage.output > 0);
    }
    // 首条输入应含 system 提示词与用户消息
    assert_eq!(records[0].input[0].role, "system");
    assert!(
        records[0].input.iter().any(|m| m.role == "user"
            && m.content
                .as_deref()
                .is_some_and(|c| c.contains("mock 文件"))),
        "输入投影应含用户消息"
    );
    // 增量落盘：原始行不重复完整上下文（第二条起 input_offset > 0 且只存新增）；
    // read_all 展开后每条都是完整序列（次条比首条长）
    let raw = std::fs::read_to_string(&path).unwrap();
    let deltas: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(deltas.len() >= 2);
    assert_eq!(deltas[0]["input_offset"], 0, "首条全量");
    assert!(
        deltas[1]["input_offset"].as_u64().unwrap_or(0) > 0,
        "第二条起应只存增量: {:?}",
        deltas[1]["input_offset"]
    );
    assert!(
        deltas[1]["input"].as_array().map(|a| a.len()).unwrap_or(0) < records[1].input.len(),
        "原始 delta 条数应少于展开后的完整输入条数"
    );
    assert!(
        records[1].input.len() > records[0].input.len(),
        "展开后次条输入包含新增消息"
    );
    let _ = std::fs::remove_dir_all(data_dir.parent().unwrap().parent().unwrap());
}
