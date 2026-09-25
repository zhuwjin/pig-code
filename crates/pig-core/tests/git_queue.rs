mod common;

use common::{new_session, recv_until, setup};
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

fn git(dir: &std::path::Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_info_and_checkout() {
    let (config_path, cwd, data_dir) = setup("m6-git");
    if !git(&cwd, &["init"]) {
        eprintln!("git 不可用，跳过");
        return;
    }
    assert!(git(
        &cwd,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-m",
            "init"
        ]
    ));
    assert!(git(&cwd, &["branch", "feature-x"]));

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();

    agent
        .ops
        .send(Op::GitInfo { cwd: cwd.clone() })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::GitInfo { .. })
    })
    .await;
    let Some(Event::GitInfo {
        current_branch,
        branches,
        ..
    }) = collected.last()
    else {
        panic!("应有 GitInfo")
    };
    assert!(current_branch.is_some(), "应识别当前分支");
    assert!(
        branches.contains(&"feature-x".to_string()),
        "分支列表: {branches:?}"
    );

    // 切换分支
    agent
        .ops
        .send(Op::CheckoutBranch {
            cwd: cwd.clone(),
            branch: "feature-x".into(),
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::BranchChanged { .. })
    })
    .await;
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::BranchChanged { branch, .. } if branch == "feature-x")),
        "应切换成功: {collected:#?}"
    );
    let (_, head) = pig_core::git::git_info(&cwd);
    let _ = head;
    let (current, _) = pig_core::git::git_info(&cwd);
    assert_eq!(current.as_deref(), Some("feature-x"));

    // 非仓库报错
    let nonrepo = std::env::temp_dir().join(format!("pig-core-nogit-{}", std::process::id()));
    std::fs::create_dir_all(&nonrepo).unwrap();
    let (current, branches) = pig_core::git::git_info(&nonrepo);
    assert!(current.is_none() && branches.is_empty(), "非 git 仓库");
    let err = pig_core::git::checkout(&nonrepo, "x");
    assert!(err.is_err(), "非仓库 checkout 应报错");

    agent.shutdown();
}

/// 回合中发消息 → 排队 → 回合结束自动接续。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_queue_fifo() {
    let (config_path, cwd, data_dir) = setup("m6-queue");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: "读一下 mock 文件并总结".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();

    // 等第一个回合开始后再发第二条
    recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnStarted { .. })
    })
    .await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: "ECHO_HISTORY".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();

    // 应收到 MessageQueued，然后两个 TurnComplete
    let mut queued = false;
    let mut completes = 0;
    let mut count: Option<usize> = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            assert!(std::time::Instant::now() < deadline, "排队流程超时");
            continue;
        };
        match &event {
            Event::MessageQueued { text, .. } if text == "ECHO_HISTORY" => queued = true,
            Event::TextDone { full_text, .. } => {
                if let Some(n) = full_text.strip_prefix("HISTORY_COUNT:") {
                    count = n.parse().ok();
                }
            }
            Event::TurnComplete { .. } => {
                completes += 1;
                if completes == 2 {
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(queued, "应收到 MessageQueued");
    // 第二轮历史 = system + (user+assistant+tool+assistant) + user = 6
    assert_eq!(count, Some(6), "第二轮应携带完整历史");

    agent.shutdown();
}
