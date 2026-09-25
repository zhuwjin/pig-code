//! 工作区外读/写开关（fs_read_outside / fs_write_outside）+ tmp 目录放行 +
//! 区外 symlink/敏感文件语义 + Plan 模式正交性。

mod common;

use std::sync::atomic::Ordering;

use pig_core::provider::ToolCall;
use pig_core::task::SessionToolState;
use pig_core::tool::{self, ChangeTracker, ToolContext};
use pig_protocol::{Event, ExecMode, Op};

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "t1".into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pig-core-fs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

async fn run(
    dir: &std::path::Path,
    tracker: &mut ChangeTracker,
    state: &SessionToolState,
    name: &str,
    args: serde_json::Value,
) -> (String, bool) {
    let (out, is_error, _, _, _) = tool::execute(
        &call(name, args),
        ToolContext {
            cwd: dir,
            tracker,
            state,
        },
    )
    .await;
    (out, is_error)
}

/// 区外文件：workspace 的父目录里（相对逃逸目标，非绝对 tmp 请求）
fn outside_file(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    dir.parent()
        .unwrap()
        .join(format!("{name}-{}", std::process::id()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_blocked_by_default_with_menu_hint() {
    let dir = temp_dir("default-off");
    let target = outside_file(&dir, "outside-off.txt");
    std::fs::write(&target, "secret\n").unwrap();
    let rel = format!("../{}", target.file_name().unwrap().to_string_lossy());
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": rel}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("越出工作目录"), "{out}");
    assert!(out.contains("允许读取工作区外文件"), "读引导文案: {out}");

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": rel, "content": "x\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("越出工作目录"), "{out}");
    assert!(out.contains("允许写入工作区外文件"), "写引导文案: {out}");

    let _ = std::fs::remove_file(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_allowed_when_switch_on() {
    let dir = temp_dir("switch-on");
    let target = outside_file(&dir, "outside-on.txt");
    let rel = format!("../{}", target.file_name().unwrap().to_string_lossy());
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    state.fs_write_outside.store(true, Ordering::Relaxed);
    state.fs_read_outside.store(true, Ordering::Relaxed);

    // 区外写（新建）成功
    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": rel, "content": "hello\n"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello\n");

    // 区外读成功
    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": rel}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("hello"), "{out}");

    let _ = std::fs::remove_file(&target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tmp_absolute_path_allowed_with_switches_off() {
    let dir = temp_dir("tmp-ok");
    // 显式绝对路径指向系统 tmp（工作区外）：开关全关也放行
    let target =
        std::env::temp_dir().join(format!("pig-core-tmp-scratch-{}.txt", std::process::id()));
    let target_str = target.to_string_lossy().to_string();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": target_str, "content": "scratch\n"}),
    )
    .await;
    assert!(!is_error, "tmp 写放行: {out}");

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": target_str}),
    )
    .await;
    assert!(!is_error, "tmp 读放行: {out}");
    assert!(out.contains("scratch"), "{out}");

    let _ = std::fs::remove_file(&target);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_symlink_follows_the_switch() {
    let dir = temp_dir("symlink-gate");
    let outside = temp_dir("symlink-gate-outside");
    std::fs::write(outside.join("data.txt"), "linked\n").unwrap();
    std::os::unix::fs::symlink(outside.join("data.txt"), dir.join("alias.txt")).unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // 关：拒绝
    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "alias.txt"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("越出工作目录"), "{out}");

    // 开：同一个闸放行（含 symlink 指向区外）
    state.fs_read_outside.store(true, Ordering::Relaxed);
    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "alias.txt"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("linked"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_sensitive_file_still_blocked_with_switch_on() {
    let dir = temp_dir("sensitive-outside");
    let outside = outside_file(&dir, "sens-dir");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join(".env"), "SECRET=1\n").unwrap();
    let rel = format!("../{}/.env", outside.file_name().unwrap().to_string_lossy());
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    state.fs_read_outside.store(true, Ordering::Relaxed);
    state.fs_write_outside.store(true, Ordering::Relaxed);

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": rel}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("敏感文件"), "开关开后敏感文件仍拒: {out}");
    assert!(!out.contains("SECRET=1"), "{out}");

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": rel, "content": "x\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("敏感文件"), "{out}");

    let _ = std::fs::remove_dir_all(&outside);
}

/// Plan 模式硬拒与区外写开关正交：开关全开，Plan 下 Write 仍被整类硬拒。
/// 顺带走 Op::SetFsAccess 链路（agent_loop 处理点）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_blocks_write_even_with_switch_on() {
    let (config_path, cwd, data_dir) = common::setup("fs-plan");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, cwd.clone()).await;

    // 开关全开 + Plan 模式
    agent
        .ops
        .send(Op::SetFsAccess {
            session_id: session_id.clone(),
            read_outside: true,
            write_outside: true,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SetExecMode {
            session_id: session_id.clone(),
            mode: ExecMode::Plan,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{} 改个文件", pig_core::mock::SCENARIO_B_TRIGGER),
            files: vec![],
            mode: ExecMode::Plan,
        })
        .await
        .unwrap();

    let events = common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ApprovalRequested { .. })),
        "Plan 下不应弹审批"
    );
    let ends: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCallEnd {
                output, is_error, ..
            } => Some((*is_error).then_some(output.as_str())).flatten(),
            _ => None,
        })
        .collect();
    assert!(!ends.is_empty(), "工具调用发生了（被硬拒）");
    assert!(
        ends.iter().all(|out| out.contains("计划模式")),
        "全部被 Plan 硬拒: {ends:?}"
    );
    assert!(
        !cwd.join(pig_core::mock::SCENARIO_B_FILE).exists(),
        "Plan 下不应创建文件"
    );
    agent.shutdown();
}
