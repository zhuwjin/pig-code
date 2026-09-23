mod common;

use common::{new_session, recv_until, setup};
use pig_protocol::{Event, Op};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspaces_add_list_remove() {
    let (config_path, cwd, data_dir) = setup("m7-workspaces");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let _sid = new_session(&agent, cwd.clone()).await;

    // 初始为空
    agent.ops.send(Op::ListWorkspaces).await.unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::WorkspaceList { .. })
    })
    .await;
    let Some(Event::WorkspaceList { workspaces }) = collected.last() else {
        panic!()
    };
    assert!(workspaces.is_empty(), "初始工作区列表应为空");

    let p1 = cwd.join("proj-a");
    let p2 = cwd.join("proj-b");
    std::fs::create_dir_all(&p1).unwrap();
    std::fs::create_dir_all(&p2).unwrap();

    agent
        .ops
        .send(Op::AddWorkspace { path: p1.clone() })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::AddWorkspace { path: p2.clone() })
        .await
        .unwrap();
    // 重复 add 幂等
    agent
        .ops
        .send(Op::AddWorkspace { path: p1.clone() })
        .await
        .unwrap();

    let mut last = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), events.recv()).await
        else {
            continue;
        };
        if let Event::WorkspaceList { workspaces } = event {
            last = Some(workspaces);
        }
        if last.as_ref().is_some_and(|p| p.len() == 2) {
            break;
        }
    }
    let workspaces = last.expect("应有 WorkspaceList");
    assert_eq!(workspaces.len(), 2, "重复 add 应幂等: {workspaces:?}");
    assert!(workspaces.iter().any(|p| p.path == p1 && p.added_at > 0));

    // remove 存在的 + remove 不存在的（不炸）：remove = 置为隐藏，条目保留
    agent
        .ops
        .send(Op::RemoveWorkspace { path: p1.clone() })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::RemoveWorkspace {
            path: cwd.join("nonexistent"),
        })
        .await
        .unwrap();

    let mut last = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), events.recv()).await
        else {
            continue;
        };
        if let Event::WorkspaceList { workspaces } = event {
            last = Some(workspaces);
        }
        if last
            .as_ref()
            .is_some_and(|ps| ps.iter().any(|p| p.path == p1 && p.hidden))
        {
            break;
        }
    }
    let workspaces = last.unwrap();
    let visible: Vec<_> = workspaces.iter().filter(|p| !p.hidden).collect();
    assert_eq!(visible.len(), 1, "移除后仅 p2 可见: {workspaces:?}");
    assert_eq!(visible[0].path, p2);
    assert!(
        workspaces.iter().any(|p| p.path == p1 && p.hidden),
        "p1 应为隐藏条目"
    );

    // 落盘验证：重开 store，隐藏条目保留（含 proj-a）
    let store = pig_core::store::Store::open(&data_dir).unwrap();
    let persisted = store.workspaces();
    assert!(
        persisted.iter().any(|p| p.path == p1 && p.hidden)
            && persisted.iter().any(|p| p.path == p2 && !p.hidden),
        "{persisted:?}"
    );

    // 隐藏工作区下新建会话 → 自动恢复显示
    agent
        .ops
        .send(Op::RemoveWorkspace { path: cwd.clone() })
        .await
        .unwrap();
    let _sid2 = new_session(&agent, cwd.clone()).await;
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(
            e,
            Event::WorkspaceList { workspaces }
            if workspaces.iter().any(|p| p.path == cwd && !p.hidden)
        )
    })
    .await;
    assert!(collected.last().is_some(), "隐藏工作区下新建会话应恢复显示");

    agent.shutdown();
}
