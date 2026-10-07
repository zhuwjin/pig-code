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

    // Initially empty
    agent.ops.send(Op::ListWorkspaces).await.unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::WorkspaceList { .. })
    })
    .await;
    let Some(Event::WorkspaceList { workspaces }) = collected.last() else {
        panic!()
    };
    assert!(
        workspaces.is_empty(),
        "initial workspace list should be empty"
    );

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
    // Duplicate add is idempotent
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
    let workspaces = last.expect("should receive WorkspaceList");
    assert_eq!(
        workspaces.len(),
        2,
        "duplicate add should be idempotent: {workspaces:?}"
    );
    assert!(workspaces.iter().any(|p| p.path == p1 && p.added_at > 0));

    // Remove an existing path + a nonexistent one (must not blow up): remove = mark hidden, entry kept
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
    assert_eq!(
        visible.len(),
        1,
        "only p2 should remain visible after remove: {workspaces:?}"
    );
    assert_eq!(visible[0].path, p2);
    assert!(
        workspaces.iter().any(|p| p.path == p1 && p.hidden),
        "p1 should be a hidden entry"
    );

    // Persistence check: reopen the store, hidden entries are kept (including proj-a)
    let store = pig_core::store::Store::open(&data_dir).unwrap();
    let persisted = store.workspaces();
    assert!(
        persisted.iter().any(|p| p.path == p1 && p.hidden)
            && persisted.iter().any(|p| p.path == p2 && !p.hidden),
        "{persisted:?}"
    );

    // Create a session under a hidden workspace -> visibility auto-restored
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
    assert!(
        collected.last().is_some(),
        "creating a session in a hidden workspace should restore visibility"
    );

    agent.shutdown();
}
