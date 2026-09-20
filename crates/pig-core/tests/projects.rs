mod common;

use common::{new_session, recv_until, setup};
use pig_protocol::{Event, Op};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projects_add_list_remove() {
    let (config_path, cwd, data_dir) = setup("m7-projects");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let _sid = new_session(&agent, cwd.clone()).await;

    // 初始为空
    agent.ops.send(Op::ListProjects).await.unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::ProjectList { .. })
    })
    .await;
    let Some(Event::ProjectList { projects }) = collected.last() else {
        panic!()
    };
    assert!(projects.is_empty(), "初始项目列表应为空");

    let p1 = cwd.join("proj-a");
    let p2 = cwd.join("proj-b");
    std::fs::create_dir_all(&p1).unwrap();
    std::fs::create_dir_all(&p2).unwrap();

    agent.ops.send(Op::AddProject { path: p1.clone() }).await.unwrap();
    agent.ops.send(Op::AddProject { path: p2.clone() }).await.unwrap();
    // 重复 add 幂等
    agent.ops.send(Op::AddProject { path: p1.clone() }).await.unwrap();

    let mut last = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), events.recv()).await
        else {
            continue;
        };
        if let Event::ProjectList { projects } = event {
            last = Some(projects);
        }
        if last.as_ref().is_some_and(|p| p.len() == 2) {
            break;
        }
    }
    let projects = last.expect("应有 ProjectList");
    assert_eq!(projects.len(), 2, "重复 add 应幂等: {projects:?}");
    assert!(projects.iter().any(|p| p.path == p1 && p.added_at > 0));

    // remove 存在的 + remove 不存在的（不炸）
    agent.ops.send(Op::RemoveProject { path: p1.clone() }).await.unwrap();
    agent
        .ops
        .send(Op::RemoveProject {
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
        if let Event::ProjectList { projects } = event {
            last = Some(projects);
        }
        if last.as_ref().is_some_and(|p| p.len() == 1) {
            break;
        }
    }
    let projects = last.unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].path, p2);

    // 落盘验证
    let raw = std::fs::read_to_string(data_dir.join("projects.json")).unwrap();
    assert!(raw.contains("proj-b") && !raw.contains("proj-a"), "{raw}");

    agent.shutdown();
}
