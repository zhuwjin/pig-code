mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

/// 会话分叉：两回合的会话 fork turns=1 → 新会话按冷路径打开（SessionConfigured
/// + replay），回放只含第一回合；store meta 继承模型选择、标题带「（分叉）」
/// 且 title_custom（自动命名不覆盖）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_session_truncates_and_switches() {
    let (config_path, cwd, data_dir) = setup("fork");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd.clone()).await;

    for content in ["读一下 mock 文件并总结", "再总结一遍"] {
        agent
            .ops
            .send(Op::SendMessage {
                session_id: session_id.clone(),
                content: content.into(),
                files: vec![],
                images: vec![],
                mode: ExecMode::AutoEdit,
            })
            .await
            .unwrap();
        recv_until(&events, Duration::from_secs(20), |e| {
            matches!(e, Event::TurnComplete { .. })
        })
        .await;
    }

    agent
        .ops
        .send(Op::ForkSession {
            session_id: session_id.clone(),
            turns: 1,
        })
        .await
        .unwrap();
    let configured = recv_until(
        &events,
        Duration::from_secs(10),
        |e| matches!(e, Event::SessionConfigured { session_id: id, .. } if *id != session_id),
    )
    .await;
    let Some(Event::SessionConfigured {
        session_id: fork_id,
        model,
        provider_name,
        ..
    }) = configured.last()
    else {
        panic!("分叉后应有新会话的 SessionConfigured: {configured:?}");
    };
    let fork_id = fork_id.clone();
    assert_eq!(model, "mock-model", "模型解析应继承源会话（配置默认）");
    assert_eq!(provider_name, "Mock 供应商");

    // 回放（含第一回合 TurnStats 的 TurnComplete 与 duration_ms=0 的收尾
    // TurnComplete）全部排完再断言内容
    let replay = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnComplete { stats: None, .. })
    })
    .await;
    let user_msgs: Vec<&str> = replay
        .iter()
        .filter_map(|e| match e {
            Event::UserMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_msgs.len(),
        1,
        "回放应只有第一回合的用户消息: {user_msgs:?}"
    );
    assert!(user_msgs[0].contains("读一下"));
    assert!(
        replay.iter().any(|e| matches!(e, Event::TextDone { full_text, .. } if full_text.contains(mock::MOCK_REPLY_MARKER))),
        "回放应含第一回合的助手回复"
    );
    // 第二回合的记录不得进入分叉 rollout
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{fork_id}.jsonl")))
            .unwrap();
    assert!(
        !rollout.contains("再总结一遍"),
        "分叉 rollout 不应含第二回合"
    );

    // 索引：分叉补发的 SessionList 在 C1 批里（先于 SessionConfigured 到达）
    let listed = configured
        .iter()
        .find_map(|e| match e {
            Event::SessionList { sessions } => Some(sessions),
            _ => None,
        })
        .expect("分叉应补发 SessionList");
    let meta = listed
        .iter()
        .find(|s| s.id == fork_id)
        .expect("列表应有分叉会话");
    assert!(
        meta.title.contains("（分叉）"),
        "标题应带分叉后缀: {}",
        meta.title
    );
    assert!(meta.title_custom, "分叉标题应固定（自动命名不覆盖）");
    assert!(!meta.pinned, "置顶不继承");

    agent.shutdown();
}
