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
    let (out, is_error, _, _, _) = tool::execute(
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
    let (out, is_error, _, _, _) = tool::execute(
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

    // 按实际 shell 选方言：Git Bash 用 $VAR，回退 cmd 才用 %VAR%；
    // 两种写法展开后的输出一致，断言共用。
    let cmd_dialect = cfg!(windows)
        && matches!(
            pig_core::task::windows_shell(),
            pig_core::task::WindowsShell::Cmd
        );
    let echo_env = if cmd_dialect {
        "echo gtp=%GIT_TERMINAL_PROMPT% nc=%NO_COLOR% term=%TERM% pyio=%PYTHONIOENCODING%"
    } else {
        "echo gtp=$GIT_TERMINAL_PROMPT nc=$NO_COLOR term=$TERM pyio=$PYTHONIOENCODING"
    };
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": echo_env}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("gtp=0"), "GIT_TERMINAL_PROMPT=0: {out}");
    assert!(out.contains("nc=1"), "NO_COLOR=1: {out}");
    assert!(out.contains("term=dumb"), "TERM=dumb: {out}");
    assert!(out.contains("pyio=utf-8"), "PYTHONIOENCODING=utf-8: {out}");
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
async fn dangerous_command_not_blocked_at_execute_layer() {
    let dir = temp_dir("danger-exec");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // 黑名单拦截已上移到会话层（强制审批弹窗）；execute 层不再硬拒。
    // mkfs 命中黑名单但执行无害：macOS 无此命令（exit 127），Linux 无参数只打印用法。
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "mkfs"}),
    )
    .await;
    assert!(!is_error, "execute 层不再拦截: {out}");
    assert!(out.contains("[exit code:"), "命令真正走了执行路径: {out}");

    // 后台路径同样不拦
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "mkfs", "run_in_background": true}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("task_id"), "{out}");
}

// ---------- 只读命令白名单（AutoEdit 直通） ----------

#[test]
fn readonly_command_whitelist() {
    let dir = temp_dir("readonly");
    // 简单命令:白名单 + git 子命令
    for cmd in [
        "ls",
        "ls -la src",
        "git status",
        "git log --oneline -5",
        "git diff HEAD~1",
        "git show HEAD",
        "git branch",
        "git remote",
        "git tag",
        "rg TODO",
        "wc -l f.txt",
        "echo hello",
    ] {
        assert!(tool::is_readonly_command(cmd, &dir), "{cmd} 应放行");
    }
    // 吐文件类命令(参数级判定):工作区内普通文件放行
    for cmd in [
        "cat Cargo.toml",
        "cat src/a.rs src/b.rs",
        "cat .env.example", // 模板类豁免(is_sensitive_file 同口径)
        "head -20 a.rs",
        "head -n 50 app.log",
        "head --lines=5 log",
        "tail -n 50 app.log",
        "tail --lines 5 log",
        "sort names.txt",
        "sort -u names.txt",
        "uniq dedup.txt",
    ] {
        assert!(tool::is_readonly_command(cmd, &dir), "{cmd} 应放行");
    }
    for cmd in [
        // 形态门
        "ls > files.txt", // 重定向
        "cat a | grep x", // 管道
        "ls && pwd",      // 链式
        "ls; pwd",        // 分号
        "echo `date`",    // 反引号命令替换
        "echo $(date)",   // $(…) 命令替换
        "ls\npwd",        // 多行
        // 吐文件类:敏感 / 越界 / glob / stdin / follow / 写输出 → 拒绝
        "cat .env",
        "cat config/.env.local",
        "cat .ssh/id_rsa",
        "cat ../outside.txt",
        "cat /etc/hosts", // 绝对路径区外(Windows 下 MSYS /x 直接拒)
        "head -20 src/*.rs",
        "cat",          // 无文件参数 = stdin
        "cat -",        // stdin
        "head -n .env", // 敏感路径落进选项值位也因「无文件参数」被拒
        "tail -f app.log",
        "tail --follow log",
        "sort in.txt -o out.txt",
        "sort --output=out.txt in.txt",
        "uniq in.txt out.txt", // 第二位置参数是输出文件
        // git / 其余命令
        "git branch -D feature", // 带参子命令（删除分支）
        "git push",              // 非只读子命令
        "git checkout main",     //
        "cargo check",           // 构建命令会写 target/
        "npm install",           //
        "rm -rf node_modules",   //
        "make",                  //
    ] {
        assert!(!tool::is_readonly_command(cmd, &dir), "{cmd} 不应放行");
    }
}

// ---------- 快照可见性：前台执行中不上屏、转后台/注册即上屏 ----------

/// 带真实 notify channel 的 state（for_test 的 receiver 直接丢弃，收不到通知）
fn notify_state() -> (
    SessionToolState,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (wake_tx, _wake_rx) = tokio::sync::mpsc::unbounded_channel();
    (
        SessionToolState::new("test".into(), tx, wake_tx, vec![]),
        rx,
    )
}

/// 后台任务执行期间跑普通前台命令：notify 落在执行窗口内，
/// 快照不得把前台命令当「后台 Bash · 运行中」推上屏。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_command_not_leaked_into_snapshot() {
    let dir = temp_dir("fg-snapshot");
    let (state, mut rx) = notify_state();

    // 后台任务 0.2s 后退出 → 它的 watcher notify 会落进前台命令执行窗口
    let bg_id = pig_core::task::spawn_background(&state, &dir, "sleep 0.2");
    rx.recv().await.expect("spawn_background 注册即 notify");

    let fg_state = state.clone();
    let fg_dir = dir.clone();
    let fg = tokio::spawn(async move {
        pig_core::task::run_foreground(
            &fg_state,
            &fg_dir,
            "sleep 1 && echo fg",
            std::time::Duration::from_secs(10),
        )
        .await
    });

    // 后台退出 notify 到达时 agent_loop 会推此刻的快照
    rx.recv().await.expect("后台退出应 notify");
    let snap = pig_core::task::snapshot(&state.tasks);
    assert!(
        snap.iter().any(|t| t.id == bg_id),
        "后台任务应在快照: {snap:?}"
    );
    assert!(
        snap.iter().all(|t| t.command != "sleep 1 && echo fg"),
        "前台命令不得泄漏进快照: {snap:?}"
    );

    // 前台正常完成（条目移除不留痕）
    let outcome = fg.await.expect("fg join");
    assert!(matches!(
        outcome,
        pig_core::task::ForegroundOutcome::Completed { .. }
    ));
}

/// 前台超时转后台：条目立即对快照可见（Running）并 notify 上屏。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_conversion_visible_and_notified() {
    let dir = temp_dir("fg-convert");
    let (state, mut rx) = notify_state();

    let outcome =
        pig_core::task::run_foreground(&state, &dir, "sleep 5", std::time::Duration::from_secs(1))
            .await;
    let pig_core::task::ForegroundOutcome::TimedOut { task_id } = outcome else {
        panic!("sleep 5 限 1s 应超时转后台")
    };

    // 转后台即刻 notify + 快照可见 Running
    rx.recv().await.expect("转后台应 notify");
    let snap = pig_core::task::snapshot(&state.tasks);
    let entry = snap
        .iter()
        .find(|t| t.id == task_id)
        .expect("转后台条目应立即可见: {snap:?}");
    assert!(matches!(entry.status, pig_protocol::TaskStatus::Running));

    // 收尾杀掉，不留 sleep 进程
    let mut tracker = ChangeTracker::default();
    let (out, is_error) = task_ctl(&dir, &mut tracker, &state, "TaskStop", &task_id).await;
    assert!(!is_error, "{out}");
}

/// 前台执行中取消（点停止 → exec_tool_gated 的 select! drop 掉工具 future）：
/// guard 收尾移除注册表条目与 spill，不残留「运行中」孤儿。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_foreground_leaves_no_trace() {
    let dir = temp_dir("fg-cancel");
    let (state, mut rx) = notify_state();

    let fg_state = state.clone();
    let fg_dir = dir.clone();
    let handle = tokio::spawn(async move {
        pig_core::task::run_foreground(
            &fg_state,
            &fg_dir,
            "sleep 5",
            std::time::Duration::from_secs(10),
        )
        .await
    });
    // 等注册完成（future 已在 child.wait() 上挂起）
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // 模拟取消：drop 掉 run_foreground future（abort 即 drop）
    handle.abort();
    let _ = handle.await;

    // guard 收尾：notify 到达 + 注册表/快照无残留
    rx.recv().await.expect("取消应收尾 notify");
    let raw_empty = state.tasks.lock().expect("tasks lock").is_empty();
    assert!(raw_empty, "注册表不应有孤儿条目");
    let snap = pig_core::task::snapshot(&state.tasks);
    assert!(snap.is_empty(), "取消后快照应为空: {snap:?}");
}

/// 前台狂喷输出（seq ≈ 23MB > 16MiB）：超限强停，完成的输出尾部带说明。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_output_cap_kills_command() {
    let dir = temp_dir("cap16m-fg");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "seq 1 3000000", "timeout": 120}),
    )
    .await;
    assert!(!is_error, "强停不是错误: {out}");
    assert!(out.contains("16MiB 上限"), "应说明上限: {out}");
    assert!(out.contains("强制停止"), "尾部预览应含强停说明: {out}");
}

/// 后台同样强停：条目转 Killed（watcher 不覆写），TaskOutput 尾部可见说明。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_output_cap_kills_and_notes() {
    let dir = temp_dir("cap16m-bg");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "seq 1 3000000", "run_in_background": true}),
    )
    .await;
    assert!(!is_error, "{out}");
    let task_id = out
        .strip_prefix("已在后台启动，task_id: ")
        .and_then(|rest| rest.split('。').next())
        .expect("后台文案含 task_id")
        .to_string();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let killed = {
            let tasks = state.tasks.lock().expect("tasks lock");
            tasks
                .iter()
                .any(|t| t.id == task_id && matches!(t.status, pig_protocol::TaskStatus::Killed))
        };
        if killed {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "未在期限内强停");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let (out, is_error) = task_ctl(&dir, &mut tracker, &state, "TaskOutput", &task_id).await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("强制停止"),
        "注册表 output 尾部应留说明: {out}"
    );
}

/// Git Bash 方言生效：pwd 输出 MSYS 路径（/c/... 形式），且 coreutils 可用。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_bash_dialect_when_available() {
    if !cfg!(windows)
        || !matches!(
            pig_core::task::windows_shell(),
            pig_core::task::WindowsShell::GitBash(_)
        )
    {
        return; // 回退 cmd 的机器跳过：行为与既有 cmd 用例一致
    }
    let dir = temp_dir("msys");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "pwd && printf 'shell:ok\\n'"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(
        out.lines().next().is_some_and(|l| l.starts_with('/')),
        "pwd 应输出 MSYS 路径: {out}"
    );
    assert!(out.contains("shell:ok"), "printf 可用: {out}");
}

/// GBK 回退解码：printf 输出原始 GBK 字节（D6 D0 = 「中」），
/// StreamDecoder 应按 GBK 解出而不是替换字符。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gbk_fallback_decodes_native_output() {
    if !cfg!(windows)
        || !matches!(
            pig_core::task::windows_shell(),
            pig_core::task::WindowsShell::GitBash(_)
        )
    {
        return; // 回退 cmd 无 printf；非 Windows 走 lossy 不适用
    }
    let dir = temp_dir("gbk");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        // Rust 层双反斜杠 → shell 收到字面 \xd6 文本，printf 转成原始字节 D6 D0（GBK「中」）
        serde_json::json!({"command": "printf 'prefix: \\xd6\\xd0\\n'"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("prefix: 中"), "GBK 字节应整段回退解码: {out}");
    assert!(!out.contains("\u{fffd}"), "不应出现替换字符: {out}");
}
