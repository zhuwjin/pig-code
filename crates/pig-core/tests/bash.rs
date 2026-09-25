//! Bash 前台/后台健壮性：环境注入、超时自动转后台、输出落盘 spill、进程组树杀。

use pig_core::provider::ToolCall;
use pig_core::task::SessionToolState;
use pig_core::tool::{self, ChangeTracker, ToolContext};

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "t1".into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pig-core-bash-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

async fn bash(
    dir: &std::path::Path,
    tracker: &mut ChangeTracker,
    state: &SessionToolState,
    args: serde_json::Value,
) -> (String, bool) {
    let (out, is_error, _, _) = tool::execute(
        &call("Bash", args),
        ToolContext {
            cwd: dir,
            tracker,
            state,
        },
    )
    .await;
    (out, is_error)
}

async fn task_ctl(
    dir: &std::path::Path,
    tracker: &mut ChangeTracker,
    state: &SessionToolState,
    name: &str,
    task_id: &str,
) -> (String, bool) {
    let (out, is_error, _, _) = tool::execute(
        &call(name, serde_json::json!({"task_id": task_id})),
        ToolContext {
            cwd: dir,
            tracker,
            state,
        },
    )
    .await;
    (out, is_error)
}

/// 从「已转入后台任务 bN（」文案里抠 task_id
fn timed_out_task_id(out: &str) -> String {
    out.split("已转入后台任务 ")
        .nth(1)
        .and_then(|rest| rest.split('（').next())
        .expect("超时文案应含 task_id")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_env_injection() {
    let dir = temp_dir("env");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "echo gtp=$GIT_TERMINAL_PROMPT nc=$NO_COLOR term=$TERM"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("gtp=0"), "GIT_TERMINAL_PROMPT=0: {out}");
    assert!(out.contains("nc=1"), "NO_COLOR=1: {out}");
    assert!(out.contains("term=dumb"), "TERM=dumb: {out}");
    assert!(out.contains("[exit code: 0]"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_timeout_moves_to_background() {
    let dir = temp_dir("timeout");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "sleep 30", "timeout": 1}),
    )
    .await;
    assert!(!is_error, "超时转后台不是错误: {out}");
    assert!(out.contains("已转入后台任务"), "{out}");
    let task_id = timed_out_task_id(&out);

    // 注册表里仍是 Running
    {
        let tasks = state.tasks.lock().expect("tasks lock");
        let entry = tasks.iter().find(|t| t.id == task_id).expect("任务在册");
        assert!(
            matches!(entry.status, pig_protocol::TaskStatus::Running),
            "应为 Running: {:?}",
            entry.status
        );
        assert!(entry.pid.is_some());
    }

    let (out, is_error) = task_ctl(&dir, &mut tracker, &state, "TaskStop", &task_id).await;
    assert!(!is_error, "{out}");
    {
        let tasks = state.tasks.lock().expect("tasks lock");
        let entry = tasks.iter().find(|t| t.id == task_id).unwrap();
        assert!(
            matches!(entry.status, pig_protocol::TaskStatus::Killed),
            "停止后应 Killed: {:?}",
            entry.status
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_task_completes_naturally() {
    let dir = temp_dir("timeout-done");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, _) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "sleep 2 && echo done", "timeout": 1}),
    )
    .await;
    let task_id = timed_out_task_id(&out);

    // 等 watcher 置 Exited(0)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let done = {
            let tasks = state.tasks.lock().expect("tasks lock");
            matches!(
                tasks.iter().find(|t| t.id == task_id).map(|t| &t.status),
                Some(pig_protocol::TaskStatus::Exited(0))
            )
        };
        if done {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "超时任务未完成");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let (out, is_error) = task_ctl(&dir, &mut tracker, &state, "TaskOutput", &task_id).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("done"), "转后台后输出仍在: {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_large_output_spills_to_disk() {
    let dir = temp_dir("spill");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "seq 1 20000"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.starts_with("1\n2\n3\n"), "头 4096 字符预览: {out}");
    assert!(out.contains("[...中间省略...]"), "{out}");
    assert!(out.contains("20000"), "尾 1024 字符预览: {out}");
    assert!(
        out.contains("已保存到 .pigcode/tool-results/"),
        "相对 cwd 的 spill 路径: {out}"
    );
    assert!(out.contains("[exit code: 0]"), "{out}");

    // spill 文件全量保留（b1.log）
    let spill = dir.join(".pigcode/tool-results/b1.log");
    let full = std::fs::read_to_string(&spill).expect("spill 文件存在");
    assert!(full.starts_with("1\n2\n3\n"), "spill 是全量: 头部");
    assert!(full.ends_with("20000\n"), "spill 是全量: 尾部");
    assert!(full.len() > 30 * 1024, "spill 未截断: {}", full.len());

    // 小输出命令执行后 spill 文件被清理（b2.log 不存）
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "echo small"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("small"), "{out}");
    assert!(
        !dir.join(".pigcode/tool-results/b2.log").exists(),
        "小输出不留 spill 痕"
    );
    assert!(spill.exists(), "大输出的 spill 保留");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_kills_process_group() {
    let dir = temp_dir("pgroup");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // 后台跑「子 shell 再起孙进程」：sleep 60 是 sh 的子进程
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "sleep 60 & wait", "run_in_background": true}),
    )
    .await;
    assert!(!is_error, "{out}");
    let task_id = out
        .strip_prefix("已在后台启动，task_id: ")
        .and_then(|rest| rest.split('。').next())
        .expect("后台文案含 task_id")
        .to_string();

    let (out, is_error) = task_ctl(&dir, &mut tracker, &state, "TaskStop", &task_id).await;
    assert!(!is_error, "{out}");
    let tasks = state.tasks.lock().expect("tasks lock");
    let entry = tasks.iter().find(|t| t.id == task_id).expect("任务在册");
    assert!(
        matches!(entry.status, pig_protocol::TaskStatus::Killed),
        "应为 Killed: {:?}",
        entry.status
    );
}

// ---------- 4.3 破坏性命令黑名单 ----------

#[test]
fn dangerous_command_blacklist_hits() {
    for (cmd, why) in [
        ("rm -rf /", "rm 根目录"),
        ("rm -fr ~", "rm 家目录"),
        ("rm -rf $HOME", "rm $HOME"),
        ("rm  -rf  .", "rm 当前目录"),
        ("rm -rf /*", "rm 根 glob"),
        ("sudo rm -rf /", "sudo rm 根目录"),
        ("mkfs.ext4 /dev/sda", "mkfs"),
        ("mkfs -t xfs /dev/sda", "mkfs 裸名"),
        ("fdisk /dev/sda", "fdisk"),
        ("diskutil eraseDisk APFS x /dev/disk0", "diskutil erase"),
        ("dd if=/dev/zero of=/dev/sda", "dd 写块设备"),
        ("shutdown -h now", "shutdown"),
        ("reboot", "reboot"),
        ("systemctl poweroff", "systemctl poweroff"),
        ("systemctl kexec", "systemctl kexec"),
        ("init 0", "init 0"),
        ("init 6", "init 6"),
        (":(){ :|:& };:", "fork 炸弹"),
        ("chmod -R 777 /", "chmod -R 777 /"),
        ("chown -R root /", "chown -R /"),
        ("echo ok; rm -rf /", "分号后的 rm"),
    ] {
        assert!(
            tool::is_dangerous_command(cmd).is_some(),
            "{cmd}（{why}）应拦截"
        );
    }
}

#[test]
fn dangerous_command_blacklist_allows() {
    for cmd in [
        "rm -rf node_modules",
        "rm -rf /tmp/pig-core-somedir",
        "rm -f a.txt",
        "dd if=x of=/dev/null",
        "dd if=x of=/dev/zero",
        "dd if=x of=/dev/urandom bs=1 count=4",
        "git push --force",
        "git push -f",
        "chmod 755 script.sh",
        "chmod -R 755 src",
        "echo shutdown", // 非命令位
        "echo rm -rf /", // 非命令位
        "echo mkfs",
        "ls /dev/",
    ] {
        assert!(tool::is_dangerous_command(cmd).is_none(), "{cmd} 应放行");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bash_blocks_dangerous_before_background_branch() {
    let dir = temp_dir("danger");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // 前台与后台路径都拦
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "rm -rf /"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("已拦截高风险命令"), "{out}");
    assert!(out.contains("rm -rf /"), "文案含命令预览: {out}");

    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "rm -rf /", "run_in_background": true}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("已拦截高风险命令"), "{out}");
    assert!(
        state.tasks.lock().expect("tasks lock").is_empty(),
        "被拦命令不应注册任务"
    );

    // 放行命令正常执行
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "echo safe"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("safe"), "{out}");
}
