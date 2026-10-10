//! Bash foreground/background robustness: env injection, timeout auto-conversion to background, output spill to disk, process-group tree kill.

use pig_core::task::SessionToolState;
use pig_core::tool::{self, ChangeTracker, ToolContext};
use pig_provider::ToolCall;

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
            fs_grant: None,
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
            fs_grant: None,
        },
    )
    .await;
    (out, is_error)
}

/// Extract task_id from the "moved to background task bN (" message
fn timed_out_task_id(out: &str) -> String {
    out.split("moved to background task ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .expect("timeout message should contain task_id")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_env_injection() {
    let dir = temp_dir("env");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // Pick the dialect per the actual shell: Git Bash uses $VAR, only the cmd
    // fallback uses %VAR%; both forms expand to the same output, so the
    // assertions are shared.
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
    assert!(!is_error, "timeout-to-background is not an error: {out}");
    assert!(out.contains("moved to background task"), "{out}");
    let task_id = timed_out_task_id(&out);

    // Still Running in the registry
    {
        let tasks = state.tasks.lock().expect("tasks lock");
        let entry = tasks
            .iter()
            .find(|t| t.id == task_id)
            .expect("task registered");
        assert!(
            matches!(entry.status, pig_protocol::TaskStatus::Running),
            "expected Running: {:?}",
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
            "expected Killed after stop: {:?}",
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

    // Wait for the watcher to set Exited(0)
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
        assert!(
            std::time::Instant::now() < deadline,
            "timed-out task did not finish"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let (out, is_error) = task_ctl(&dir, &mut tracker, &state, "TaskOutput", &task_id).await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("done"),
        "output preserved after moving to background: {out}"
    );
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
    assert!(
        out.starts_with("1\n2\n3\n"),
        "head 4096-char preview: {out}"
    );
    assert!(out.contains("[...middle omitted...]"), "{out}");
    assert!(out.contains("20000"), "tail 1024-char preview: {out}");
    assert!(
        out.contains("the full output was saved to .pigcode/tool-results/"),
        "spill path relative to cwd: {out}"
    );
    assert!(out.contains("[exit code: 0]"), "{out}");

    // The spill file keeps the full content (b1.log)
    let spill = dir.join(".pigcode/tool-results/b1.log");
    let full = std::fs::read_to_string(&spill).expect("spill file exists");
    assert!(
        full.starts_with("1\n2\n3\n"),
        "spill holds the full content: head"
    );
    assert!(
        full.ends_with("20000\n"),
        "spill holds the full content: tail"
    );
    assert!(
        full.len() > 30 * 1024,
        "spill not truncated: {}",
        full.len()
    );

    // After a small-output command runs, the spill file is cleaned up (b2.log absent)
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
        "small output leaves no spill trace"
    );
    assert!(spill.exists(), "large output keeps its spill");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_kills_process_group() {
    let dir = temp_dir("pgroup");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // Run "a child shell spawning a grandchild process" in the background: sleep 60 is a child of sh
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "sleep 60 & wait", "run_in_background": true}),
    )
    .await;
    assert!(!is_error, "{out}");
    let task_id = out
        .strip_prefix("Started in the background, task_id: ")
        .and_then(|rest| rest.split('.').next())
        .expect("background message contains task_id")
        .to_string();

    let (out, is_error) = task_ctl(&dir, &mut tracker, &state, "TaskStop", &task_id).await;
    assert!(!is_error, "{out}");
    let tasks = state.tasks.lock().expect("tasks lock");
    let entry = tasks
        .iter()
        .find(|t| t.id == task_id)
        .expect("task registered");
    assert!(
        matches!(entry.status, pig_protocol::TaskStatus::Killed),
        "expected Killed: {:?}",
        entry.status
    );
}

// ---------- 4.3 Destructive command blocklist ----------

#[test]
fn dangerous_command_blacklist_hits() {
    for (cmd, why) in [
        ("rm -rf /", "rm root directory"),
        ("rm -fr ~", "rm home directory"),
        ("rm -rf $HOME", "rm $HOME"),
        ("rm  -rf  .", "rm current directory"),
        ("rm -rf /*", "rm root glob"),
        ("sudo rm -rf /", "sudo rm root directory"),
        ("mkfs.ext4 /dev/sda", "mkfs"),
        ("mkfs -t xfs /dev/sda", "bare mkfs"),
        ("fdisk /dev/sda", "fdisk"),
        ("diskutil eraseDisk APFS x /dev/disk0", "diskutil erase"),
        (
            "dd if=/dev/zero of=/dev/sda",
            "dd writing to a block device",
        ),
        ("shutdown -h now", "shutdown"),
        ("reboot", "reboot"),
        ("systemctl poweroff", "systemctl poweroff"),
        ("systemctl kexec", "systemctl kexec"),
        ("init 0", "init 0"),
        ("init 6", "init 6"),
        (":(){ :|:& };:", "fork bomb"),
        ("chmod -R 777 /", "chmod -R 777 /"),
        ("chown -R root /", "chown -R /"),
        ("echo ok; rm -rf /", "rm after a semicolon"),
    ] {
        assert!(
            tool::is_dangerous_command(cmd).is_some(),
            "{cmd} ({why}) should be blocked"
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
        "echo shutdown", // not in command position
        "echo rm -rf /", // not in command position
        "echo mkfs",
        "ls /dev/",
    ] {
        assert!(
            tool::is_dangerous_command(cmd).is_none(),
            "{cmd} should be allowed"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dangerous_command_not_blocked_at_execute_layer() {
    let dir = temp_dir("danger-exec");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // Blocklist interception has moved up to the session layer (forced
    // approval popup); the execute layer no longer hard-denies.
    // mkfs hits the blocklist but is harmless to run: macOS lacks the command
    // (exit 127), Linux without args just prints usage.
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "mkfs"}),
    )
    .await;
    assert!(!is_error, "execute layer no longer blocks: {out}");
    assert!(
        out.contains("[exit code:"),
        "command actually took the execution path: {out}"
    );

    // The background path does not block either
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

// ---------- Read-only command allowlist (AutoEdit pass-through) ----------

#[test]
fn readonly_command_whitelist() {
    let dir = temp_dir("readonly");
    // Simple commands: allowlist + git subcommands
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
        assert!(
            tool::is_readonly_command(cmd, &dir),
            "{cmd} should be allowed"
        );
    }
    // File-emitting commands (argument-level check): plain in-workspace files allowed
    for cmd in [
        "cat Cargo.toml",
        "cat src/a.rs src/b.rs",
        "cat .env.example", // template exemption (same rule as is_sensitive_file)
        "head -20 a.rs",
        "head -n 50 app.log",
        "head --lines=5 log",
        "tail -n 50 app.log",
        "tail --lines 5 log",
        "sort names.txt",
        "sort -u names.txt",
        "uniq dedup.txt",
    ] {
        assert!(
            tool::is_readonly_command(cmd, &dir),
            "{cmd} should be allowed"
        );
    }
    for cmd in [
        // Shape gate
        "ls > files.txt", // redirect
        "cat a | grep x", // pipe
        "ls && pwd",      // chained
        "ls; pwd",        // semicolon
        "echo `date`",    // backtick command substitution
        "echo $(date)",   // $(…) command substitution
        "ls\npwd",        // multi-line
        // File-emitting: sensitive / out-of-bounds / glob / stdin / follow /
        // write output → deny
        "cat .env",
        "cat config/.env.local",
        "cat .ssh/id_rsa",
        "cat ../outside.txt",
        "cat /etc/hosts", // absolute path outside the workspace (MSYS /x is denied outright on Windows)
        "head -20 src/*.rs",
        "cat",          // no file argument = stdin
        "cat -",        // stdin
        "head -n .env", // a sensitive path landing in an option-value slot is also rejected due to "no file argument"
        "tail -f app.log",
        "tail --follow log",
        "sort in.txt -o out.txt",
        "sort --output=out.txt in.txt",
        "uniq in.txt out.txt", // the second positional argument is the output file
        // git / other commands
        "git branch -D feature", // subcommand with arguments (branch deletion)
        "git push",              // non-read-only subcommand
        "git checkout main",     //
        "cargo check",           // build commands write target/
        "npm install",           //
        "rm -rf node_modules",   //
        "make",                  //
    ] {
        assert!(
            !tool::is_readonly_command(cmd, &dir),
            "{cmd} should not be allowed"
        );
    }
}

// ---------- Snapshot visibility: hidden while running in the foreground, visible on background conversion/registration ----------

/// State with a real notify channel (for_test drops the receiver, so notifications never arrive)
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

/// Run a plain foreground command while a background task executes: the notify
/// lands inside the execution window, and the snapshot must not push the
/// foreground command onto the screen as "background Bash · running".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_command_not_leaked_into_snapshot() {
    let dir = temp_dir("fg-snapshot");
    let (state, mut rx) = notify_state();

    // The background task exits after 0.2s → its watcher notify lands inside the foreground command's execution window
    let bg_id = pig_core::task::spawn_background(&state, &dir, "sleep 0.2");
    rx.recv()
        .await
        .expect("spawn_background notifies upon registration");

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

    // When the background-exit notify arrives, agent_loop pushes the snapshot at that moment
    rx.recv().await.expect("background exit should notify");
    let snap = pig_core::task::snapshot(&state.tasks);
    assert!(
        snap.iter().any(|t| t.id == bg_id),
        "background task should be in the snapshot: {snap:?}"
    );
    assert!(
        snap.iter().all(|t| t.command != "sleep 1 && echo fg"),
        "foreground command must not leak into the snapshot: {snap:?}"
    );

    // The foreground command completes normally (entry removed without a trace)
    let outcome = fg.await.expect("fg join");
    assert!(matches!(
        outcome,
        pig_core::task::ForegroundOutcome::Completed { .. }
    ));
}

/// Foreground timeout converts to background: the entry becomes visible to the snapshot immediately (Running) and a notify pushes it to the screen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_conversion_visible_and_notified() {
    let dir = temp_dir("fg-convert");
    let (state, mut rx) = notify_state();

    let outcome =
        pig_core::task::run_foreground(&state, &dir, "sleep 5", std::time::Duration::from_secs(1))
            .await;
    let pig_core::task::ForegroundOutcome::TimedOut { task_id } = outcome else {
        panic!("sleep 5 with a 1s limit should time out to background")
    };

    // Background conversion notifies immediately + the snapshot shows Running
    rx.recv()
        .await
        .expect("conversion to background should notify");
    let snap = pig_core::task::snapshot(&state.tasks);
    let entry = snap
        .iter()
        .find(|t| t.id == task_id)
        .expect("background-converted entry should be visible immediately: {snap:?}");
    assert!(matches!(entry.status, pig_protocol::TaskStatus::Running));

    // Kill it in cleanup; leave no sleep process behind
    let mut tracker = ChangeTracker::default();
    let (out, is_error) = task_ctl(&dir, &mut tracker, &state, "TaskStop", &task_id).await;
    assert!(!is_error, "{out}");
}

/// Cancel while running in the foreground (pressing stop → exec_tool_gated's
/// select! drops the tool future): the guard cleanup removes the registry
/// entry and spill, leaving no "running" orphan.
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
    // Wait for registration to finish (the future is already pending on child.wait())
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Simulate cancellation: drop the run_foreground future (abort is a drop)
    handle.abort();
    let _ = handle.await;

    // Guard cleanup: the notify arrives + registry/snapshot hold no leftovers
    rx.recv()
        .await
        .expect("cancel should trigger a cleanup notify");
    let raw_empty = state.tasks.lock().expect("tasks lock").is_empty();
    assert!(raw_empty, "registry should have no orphan entries");
    let snap = pig_core::task::snapshot(&state.tasks);
    assert!(
        snap.is_empty(),
        "snapshot should be empty after cancel: {snap:?}"
    );
}

/// Foreground output flood (seq ≈ 23MB > 16MiB): force-stopped at the limit; the finished output tail carries an explanation.
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
    assert!(!is_error, "force stop is not an error: {out}");
    assert!(
        out.contains("16MiB limit"),
        "should mention the limit: {out}"
    );
    assert!(
        out.contains("force-stopped"),
        "tail preview should mention the force stop: {out}"
    );
}

/// The background path force-stops too: the entry turns Killed (the watcher
/// does not overwrite it), and TaskOutput's tail keeps the note.
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
        .strip_prefix("Started in the background, task_id: ")
        .and_then(|rest| rest.split('.').next())
        .expect("background message contains task_id")
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
        assert!(
            std::time::Instant::now() < deadline,
            "not force-stopped within the deadline"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let (out, is_error) = task_ctl(&dir, &mut tracker, &state, "TaskOutput", &task_id).await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("force-stopped"),
        "registry output tail should keep the note: {out}"
    );
}

/// Git Bash dialect takes effect: pwd prints an MSYS path (/c/... form) and coreutils are available.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_bash_dialect_when_available() {
    if !cfg!(windows)
        || !matches!(
            pig_core::task::windows_shell(),
            pig_core::task::WindowsShell::GitBash(_)
        )
    {
        return; // Skip on machines that fall back to cmd: behavior matches the existing cmd cases
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
        "pwd should print an MSYS path: {out}"
    );
    assert!(out.contains("shell:ok"), "printf available: {out}");
}

/// GBK fallback decoding: printf emits raw GBK bytes (D6 D0 = "中"),
/// and StreamDecoder should decode them as GBK instead of replacement
/// characters.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gbk_fallback_decodes_native_output() {
    if !cfg!(windows)
        || !matches!(
            pig_core::task::windows_shell(),
            pig_core::task::WindowsShell::GitBash(_)
        )
    {
        return; // The cmd fallback has no printf; non-Windows goes lossy, not applicable
    }
    let dir = temp_dir("gbk");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        // Double backslash at the Rust layer → the shell receives the literal \xd6 text, and printf turns it into the raw bytes D6 D0 (GBK "中")
        serde_json::json!({"command": "printf 'prefix: \\xd6\\xd0\\n'"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("prefix: 中"),
        "GBK bytes should be decoded via the fallback decoder: {out}"
    );
    assert!(
        !out.contains("\u{fffd}"),
        "no replacement character expected: {out}"
    );
}
