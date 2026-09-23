mod common;

use common::{recv_until, setup};
use pig_protocol::{Event, Op};
use std::time::Duration;

fn git(dir: &std::path::Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn commit_all(dir: &std::path::Path, message: &str) {
    assert!(git(dir, &["add", "-A"]));
    assert!(git(
        dir,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            message
        ]
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_status_lists_unstaged_staged_and_untracked() {
    let (config_path, cwd, data_dir) = setup("git-status");
    if !git(&cwd, &["init"]) {
        eprintln!("git 不可用，跳过");
        return;
    }
    std::fs::write(cwd.join("a.txt"), "l1\nl2\nl3\n").unwrap();
    commit_all(&cwd, "init");

    // 未暂存修改：+2 -1；已暂存新增：+2；未跟踪：4 行
    std::fs::write(cwd.join("a.txt"), "l1\nL2\nL3\nl3\n").unwrap();
    std::fs::write(cwd.join("b.txt"), "x\ny\n").unwrap();
    assert!(git(&cwd, &["add", "b.txt"]));
    std::fs::write(cwd.join("c.txt"), "1\n2\n3\n4\n").unwrap();

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    agent
        .ops
        .send(Op::GitStatus { cwd: cwd.clone() })
        .await
        .unwrap();
    let collected = recv_until(&agent.events, Duration::from_secs(5), |e| {
        matches!(e, Event::GitStatus { .. })
    })
    .await;
    let status = collected.iter().find_map(|e| match e {
        Event::GitStatus {
            is_git,
            unstaged,
            staged,
            ..
        } => Some((*is_git, unstaged.clone(), staged.clone())),
        _ => None,
    });
    let (is_git, unstaged, staged) = status.expect("应收到 GitStatus");
    assert!(is_git, "git 仓库应 is_git=true");

    let a = unstaged
        .iter()
        .find(|e| e.path == "a.txt")
        .expect("a.txt 未暂存");
    assert_eq!((a.status.as_str(), a.additions, a.deletions), ("M", 2, 1));
    let c = unstaged
        .iter()
        .find(|e| e.path == "c.txt")
        .expect("c.txt 未跟踪");
    assert_eq!((c.status.as_str(), c.additions, c.deletions), ("?", 4, 0));
    let b = staged
        .iter()
        .find(|e| e.path == "b.txt")
        .expect("b.txt 已暂存");
    assert_eq!((b.status.as_str(), b.additions, b.deletions), ("A", 2, 0));

    // 未暂存 diff 原文
    agent
        .ops
        .send(Op::GitDiff {
            cwd: cwd.clone(),
            path: "a.txt".to_string(),
            staged: false,
        })
        .await
        .unwrap();
    let collected = recv_until(&agent.events, Duration::from_secs(5), |e| {
        matches!(e, Event::GitDiff { .. })
    })
    .await;
    let diff = collected.iter().find_map(|e| match e {
        Event::GitDiff { diff, .. } => Some(diff.clone()),
        _ => None,
    });
    let diff = diff.expect("应收到 GitDiff");
    assert!(
        diff.contains("-l2") && diff.contains("+L2"),
        "未暂存 diff: {diff}"
    );

    // 未跟踪文件 diff：手工拼 /dev/null → 全新增
    agent
        .ops
        .send(Op::GitDiff {
            cwd: cwd.clone(),
            path: "c.txt".to_string(),
            staged: false,
        })
        .await
        .unwrap();
    let collected = recv_until(
        &agent.events,
        Duration::from_secs(5),
        |e| matches!(e, Event::GitDiff { path, .. } if path == "c.txt"),
    )
    .await;
    let diff = collected.iter().find_map(|e| match e {
        Event::GitDiff { diff, .. } => Some(diff.clone()),
        _ => None,
    });
    let diff = diff.expect("应收到 untracked GitDiff");
    assert!(
        diff.contains("+1") && diff.contains("+4") && !diff.contains("\n-"),
        "untracked 合成 diff 应全为新增: {diff}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_status_non_repo_reports_not_git() {
    let (config_path, cwd, data_dir) = setup("git-status-nongit");
    if !git(&cwd, &["--version"]) {
        eprintln!("git 不可用，跳过");
        return;
    }
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    agent
        .ops
        .send(Op::GitStatus { cwd: cwd.clone() })
        .await
        .unwrap();
    let collected = recv_until(&agent.events, Duration::from_secs(5), |e| {
        matches!(e, Event::GitStatus { .. })
    })
    .await;
    let is_git = collected.iter().find_map(|e| match e {
        Event::GitStatus { is_git, .. } => Some(*is_git),
        _ => None,
    });
    assert_eq!(is_git, Some(false), "非 git 目录应 is_git=false");
}
