//! Bash foreground/background robustness: env injection, timeout auto-conversion to background, output spill to disk, process-group tree kill.

use pig_core::task::SessionToolState;
use pig_core::tool::{self, ChangeTracker, ToolContext};
use pig_provider::ToolCall;

mod common;

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

/// A state whose task spills land in `data_dir` (the production shape: the
/// session read root is `data_dir/sessions`, the spill dir is
/// `data_dir/sessions/<session>/tool-results`).
fn state_with_data_dir(session_id: &str, data_dir: &std::path::Path) -> SessionToolState {
    let (tx, _rx) = async_channel::bounded(1024);
    SessionToolState::new(
        session_id.to_string(),
        tx,
        vec![data_dir.join("sessions")],
        vec![],
    )
    .with_data_dir(data_dir.to_path_buf())
}

/// Shell dialect the model Bash tool actually uses on this host.
///
/// Windows runs model commands through PowerShell (falling back to cmd) because
/// Git Bash cannot initialize inside the PSEC sandbox; Unix keeps bash.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// Unix model shell; Windows never selects it (Git Bash cannot run inside
    /// the PSEC token).
    #[cfg_attr(windows, allow(dead_code))]
    Bash,
    /// Constructed only inside `dialect()`'s Windows branch.
    #[cfg_attr(not(windows), allow(dead_code))]
    PowerShell,
    Cmd,
}

fn dialect() -> Dialect {
    #[cfg(windows)]
    {
        match pig_core::task::windows_sandbox_shell() {
            pig_core::task::WindowsSandboxShell::PowerShell(_) => Dialect::PowerShell,
            pig_core::task::WindowsSandboxShell::Cmd => Dialect::Cmd,
        }
    }
    #[cfg(not(windows))]
    {
        Dialect::Bash
    }
}

/// Print the integers `1..=count`, one per line.
fn counters(count: u32) -> String {
    match dialect() {
        Dialect::Bash => format!("seq 1 {count}"),
        Dialect::PowerShell => format!("[Console]::Write(((1..{count}) -join \"`n\") + \"`n\")"),
        Dialect::Cmd => format!("for /L %%i in (1,1,{count}) do @echo %%i"),
    }
}

/// Sleep, then run a follow-up command (dialect-aware chaining).
fn sleep_then(seconds: u32, follow_up: &str) -> String {
    match dialect() {
        Dialect::Bash => format!("sleep {seconds} && {follow_up}"),
        Dialect::PowerShell => format!("Start-Sleep -Seconds {seconds}; {follow_up}"),
        Dialect::Cmd => format!("ping -n {} 127.0.0.1 >nul && {follow_up}", seconds + 1),
    }
}

/// Sleep for `seconds` (dialect-aware).
fn sleepy(seconds: u32) -> String {
    match dialect() {
        Dialect::Bash => format!("sleep {seconds}"),
        Dialect::PowerShell => format!("Start-Sleep -Seconds {seconds}"),
        Dialect::Cmd => format!("ping -n {} 127.0.0.1 >nul", seconds + 1),
    }
}

/// Echo the harness runtime environment variables in the active dialect.
fn echo_runtime_env() -> String {
    match dialect() {
        Dialect::Bash => {
            "echo gtp=$GIT_TERMINAL_PROMPT nc=$NO_COLOR term=$TERM pyio=$PYTHONIOENCODING".to_string()
        }
        Dialect::PowerShell => {
            "echo \"gtp=$env:GIT_TERMINAL_PROMPT nc=$env:NO_COLOR term=$env:TERM pyio=$env:PYTHONIOENCODING\""
                .to_string()
        }
        Dialect::Cmd => {
            "echo gtp=%GIT_TERMINAL_PROMPT% nc=%NO_COLOR% term=%TERM% pyio=%PYTHONIOENCODING%"
                .to_string()
        }
    }
}

async fn bash(
    dir: &std::path::Path,
    tracker: &mut ChangeTracker,
    state: &SessionToolState,
    args: serde_json::Value,
) -> (String, bool) {
    bash_with_profile(dir, tracker, state, args, Default::default()).await
}

async fn bash_with_profile(
    dir: &std::path::Path,
    tracker: &mut ChangeTracker,
    state: &SessionToolState,
    args: serde_json::Value,
    sandbox_profile: pig_core::task::SandboxProfile,
) -> (String, bool) {
    let (out, is_error, _, _, _) = tool::execute(
        &call("ExecCommand", args),
        ToolContext {
            cwd: dir,
            tracker,
            state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile,
            additional_permissions: Default::default(),
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
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    (out, is_error)
}

/// `workdir` moves the process (and the in-shell `cd`/`Set-Location`) without
/// moving the sandbox boundary; `shell` selects the interpreter per call.
#[tokio::test]
async fn workdir_and_shell_override_are_honored() {
    let dir = temp_dir("workdir-shell");
    let sub = dir.join("nested");
    std::fs::create_dir_all(&sub).unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    let (command, shell) = if cfg!(windows) {
        ("(Get-Location).Path", "powershell")
    } else {
        ("pwd", "bash")
    };
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": command, "workdir": "nested", "shell": shell}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.to_lowercase().contains("nested"), "{out}");
}

#[cfg(windows)]
#[tokio::test]
async fn windows_rejects_git_bash_and_lists_the_allowlist() {
    let dir = temp_dir("windows-shell-allowlist");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "echo hi", "shell": "bash"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("not an available shell on Windows"), "{out}");
    assert!(out.contains("powershell"), "{out}");
}

#[cfg(not(windows))]
#[tokio::test]
async fn unix_rejects_windows_shells_and_lists_the_allowlist() {
    let dir = temp_dir("unix-shell-allowlist");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": "echo hi", "shell": "powershell"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("not an available shell"), "{out}");
    assert!(out.contains("bash"), "{out}");
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

    let echo_env = echo_runtime_env();
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
        serde_json::json!({"command": sleepy(30), "timeout": 1}),
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
        serde_json::json!({"command": sleep_then(2, "echo done"), "timeout": 1}),
    )
    .await;
    let task_id = timed_out_task_id(&out);

    // Wait for the watcher to settle the terminal state
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let done = {
            let tasks = state.tasks.lock().expect("tasks lock");
            matches!(
                tasks.iter().find(|t| t.id == task_id).map(|t| &t.status),
                Some(pig_protocol::TaskStatus::Completed)
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
        serde_json::json!({"command": counters(20000)}),
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
        out.contains("the full output was saved to"),
        "the tool reports the spill path: {out}"
    );
    assert!(
        out.contains("b1.log"),
        "the reported spill path names the task: {out}"
    );
    assert!(out.contains("[exit code: 0]"), "{out}");

    // The spill file keeps the full content, under the session data dir.
    let spill = state.spill_path("b1").expect("data dir attached");
    assert!(!spill.starts_with(&dir), "the workspace holds no spill");
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

    // A small-output command never creates a spill file.
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
        !state.spill_path("b2").expect("data dir attached").exists(),
        "small output leaves no spill trace"
    );
    assert!(spill.exists(), "large output keeps its spill");
    let _ = std::fs::remove_dir_all(state.data_dir().expect("data dir"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Threat coverage after the move: the spill used to live in the model-writable
/// workspace (`.pigcode/tool-results`), where a linked metadata path could
/// redirect the write. It now lives under the session data directory, which
/// the model cannot write; a workspace-local `.pigcode` link is irrelevant.
/// The dummy link target is created by this test only.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spill_stays_inside_the_session_data_dir_and_ignores_workspace_links() {
    use std::os::unix::fs::symlink;

    let workspace = temp_dir("spill-data-dir");
    let dummy = temp_dir("spill-data-dir-dummy");
    let data_dir = temp_dir("spill-data-dir-store");
    symlink(&dummy, workspace.join(".pigcode")).unwrap();

    let state = state_with_data_dir("s1", &data_dir);
    let mut tracker = ChangeTracker::default();
    let (out, is_error) = bash(
        &workspace,
        &mut tracker,
        &state,
        serde_json::json!({"command": counters(20000)}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("[...middle omitted...]"), "{out}");
    assert!(out.contains("20000"), "{out}");
    assert!(out.contains("b1.log"), "the tool names the spill: {out}");

    let spill = state.spill_path("b1").expect("data dir attached");
    assert!(spill.is_file(), "the large output created the spill");
    assert!(
        spill.starts_with(&data_dir),
        "the spill stays inside the data dir: {}",
        spill.display()
    );
    assert!(
        !dummy.join("tool-results").exists(),
        "the old workspace metadata link cannot receive the spill"
    );
    let full = std::fs::read_to_string(&spill).expect("spill readable");
    assert!(full.ends_with("20000\n"), "full output persisted: tail");

    // The existing session read root still admits the model's Read.
    let spill_canonical = spill.canonicalize().expect("spill canonical");
    assert!(
        state
            .extra_read_roots
            .iter()
            .any(|root| spill_canonical.starts_with(root)),
        "a session read root contains the spill: {}",
        spill.display()
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&dummy);
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// Windows counterpart of the unix workspace-link test: a junction planted at
/// `.pigcode` cannot receive the spill, because the spill lives under the
/// session data directory and never in the workspace.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spill_ignores_a_workspace_metadata_junction() {
    let workspace = temp_dir("spill-junction");
    let dummy = temp_dir("spill-junction-dummy");
    let data_dir = temp_dir("spill-junction-store");
    common::junction(&workspace.join(".pigcode"), &dummy);

    let state = state_with_data_dir("s1", &data_dir);
    let mut tracker = ChangeTracker::default();
    let (out, is_error) = bash(
        &workspace,
        &mut tracker,
        &state,
        serde_json::json!({"command": counters(20000)}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("[...middle omitted...]"), "{out}");
    assert!(out.contains("20000"), "{out}");
    assert!(out.contains("b1.log"), "the tool names the spill: {out}");

    let spill = state.spill_path("b1").expect("data dir attached");
    assert!(spill.is_file(), "the large output created the spill");
    assert!(
        spill.starts_with(&data_dir),
        "the spill stays inside the data dir: {}",
        spill.display()
    );
    assert!(
        !dummy.join("tool-results").exists(),
        "the workspace metadata junction cannot receive the spill"
    );
    let full = std::fs::read_to_string(&spill).expect("spill readable");
    assert!(full.ends_with("20000\n"), "full output persisted: tail");

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&dummy);
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// Task ids are per-session sequences ("b1"...), so two sessions in one
/// workspace both name their first task b1; the session data directories keep
/// the files apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spills_do_not_collide_across_sessions_in_one_workspace() {
    let workspace = temp_dir("spill-sessions");
    let data_dir = temp_dir("spill-sessions-store");
    let first = state_with_data_dir("s1", &data_dir);
    let second = state_with_data_dir("s2", &data_dir);
    let mut tracker = ChangeTracker::default();

    let (out, is_error) = bash(
        &workspace,
        &mut tracker,
        &first,
        serde_json::json!({"command": counters(20000)}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("[...middle omitted...]"), "{out}");

    let marker = format!("echo SESSION-TWO\n{}", counters(20000));
    let (out, is_error) = bash(
        &workspace,
        &mut tracker,
        &second,
        serde_json::json!({"command": marker}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("SESSION-TWO"), "{out}");

    let first_spill = first.spill_path("b1").expect("data dir attached");
    let second_spill = second.spill_path("b1").expect("data dir attached");
    assert_ne!(
        first_spill, second_spill,
        "the same task id in two sessions must not share a file"
    );
    let first_text = std::fs::read_to_string(&first_spill).expect("first spill");
    let second_text = std::fs::read_to_string(&second_spill).expect("second spill");
    assert!(
        !first_text.contains("SESSION-TWO"),
        "the first session's spill is not appended to"
    );
    assert!(
        second_text.contains("SESSION-TWO"),
        "the second session keeps its own output"
    );
    assert!(first_text.ends_with("20000\n"));
    assert!(second_text.ends_with("20000\n"));

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&data_dir);
}

async fn assert_stop_kills_process_group(profile: pig_core::task::SandboxProfile) {
    let dir = temp_dir("pgroup");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let started = dir.join("grandchild-started");
    let late = dir.join("grandchild-late-write");
    let command_path = |path: &std::path::Path| {
        let text = path.display().to_string();
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
    };
    let script = dir.join("grandchild.cmd");
    std::fs::write(
        &script,
        format!(
            "@echo off\r\necho started > \"{}\"\r\nping -n 2 127.0.0.1 >nul\r\necho late > \"{}\"\r\n",
            command_path(&started),
            command_path(&late)
        ),
    )
    .expect("grandchild fixture");
    let shell_path = |path: &std::path::Path| command_path(path).replace('\\', "/");
    let command = match dialect() {
        Dialect::Bash => format!(
            "(touch \"{}\"; sleep 1; printf late > \"{}\") & wait",
            shell_path(&started),
            shell_path(&late)
        ),
        Dialect::PowerShell => format!(
            "& $env:COMSPEC /c '{}'",
            command_path(&script).replace('\'', "''")
        ),
        Dialect::Cmd => format!(
            "start \"pig-grandchild\" /b cmd /c \"{}\" & ping -n 30 127.0.0.1 >nul",
            command_path(&script)
        ),
    };
    let (out, is_error) = bash_with_profile(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": command, "run_in_background": true}),
        profile,
    )
    .await;
    assert!(!is_error, "{out}");
    let task_id = out
        .strip_prefix("Started in the background, task_id: ")
        .and_then(|rest| rest.split('.').next())
        .expect("background message contains task_id")
        .to_string();
    let child_started = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !started.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        child_started.is_ok(),
        "grandchild did not start: {:?}",
        state
            .tasks
            .lock()
            .expect("tasks")
            .iter()
            .map(|task| (task.status, task.reason.clone(), task.output.clone()))
            .collect::<Vec<_>>()
    );

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
    assert!(entry.ended_at.is_some(), "stop returned before settlement");
    drop(tasks);
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(
        !late.exists(),
        "a grandchild survived TaskStop and wrote after it returned"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_kills_process_group() {
    assert_stop_kills_process_group(Default::default()).await;
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_kills_process_group_without_sandbox() {
    assert_stop_kills_process_group(pig_core::task::SandboxProfile::Unrestricted).await;
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
        "rm -r node_modules",
        "rm -r /tmp/pig-core-somedir",
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

// ---------- Snapshot visibility: hidden while running in the foreground, visible on background conversion/registration ----------

/// State with a real notify channel (for_test drops the receiver, so notifications never arrive)
fn notify_state() -> (
    SessionToolState,
    async_channel::Receiver<pig_core::task::TaskChange>,
) {
    let (tx, rx) = async_channel::bounded(1024);
    (SessionToolState::new("test".into(), tx, vec![], vec![]), rx)
}

/// Run a plain foreground command while a background task executes: the notify
/// lands inside the execution window, and the snapshot must not push the
/// foreground command onto the screen as "background Bash · running".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_command_not_leaked_into_snapshot() {
    let dir = temp_dir("fg-snapshot");
    let (state, rx) = notify_state();

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
    let (state, rx) = notify_state();

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
    let (state, rx) = notify_state();

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
        serde_json::json!({"command": counters(3_000_000), "timeout": 300}),
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
        serde_json::json!({"command": counters(3_000_000), "run_in_background": true}),
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

/// The sandboxed shell dialect takes effect and starts in the session workspace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sandbox_shell_dialect_takes_effect() {
    let dir = temp_dir("shell-dialect");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let command = match dialect() {
        Dialect::Bash => "pwd && printf 'shell:ok\\n'",
        Dialect::PowerShell => "(Get-Location).Path; Write-Output 'shell:ok'",
        Dialect::Cmd => "cd && echo shell:ok",
    };
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": command}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("shell:ok"), "{out}");
    // A native sandbox cannot open the requested cwd while creating the
    // process, so the shell resets it; the backend must re-enter the workspace
    // from the command line.
    let name = dir
        .file_name()
        .expect("workspace directory name")
        .to_string_lossy()
        .to_string();
    assert!(
        out.contains(&name),
        "shell should start in the session workspace: {out}"
    );
}

/// GBK fallback decoding: the child emits raw GBK bytes (D6 D0 = "中") and
/// StreamDecoder decodes them as GBK instead of replacement characters.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gbk_fallback_decodes_native_output() {
    if dialect() == Dialect::Cmd {
        return; // cmd has no reliable way to emit raw GBK bytes here
    }
    let dir = temp_dir("gbk");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let command = match dialect() {
        // Double backslash at the Rust layer → the shell receives the literal
        // \xd6 text, and printf turns it into the raw bytes D6 D0 (GBK "中").
        Dialect::Bash => "printf 'prefix: \\xd6\\xd0\\n'",
        // Windows PowerShell writes through the console encoding: switch it to
        // GBK (936) and let the host encode the character.
        Dialect::PowerShell => {
            "[Console]::OutputEncoding=[System.Text.Encoding]::GetEncoding(936); [Console]::WriteLine('prefix: 中')"
        }
        Dialect::Cmd => unreachable!(),
    };
    let (out, is_error) = bash(
        &dir,
        &mut tracker,
        &state,
        serde_json::json!({"command": command}),
    )
    .await;
    assert!(!is_error, "{out}");
    if cfg!(windows) {
        assert!(
            out.contains("prefix: 中"),
            "GBK bytes should be decoded via the fallback decoder: {out}"
        );
        assert!(
            !out.contains("\u{fffd}"),
            "no replacement character expected: {out}"
        );
    } else {
        // The GBK fallback is Windows-only; elsewhere invalid bytes decode
        // lossily to replacement characters.
        assert!(out.contains('\u{fffd}'), "lossy decode expected: {out}");
    }
}

#[cfg(target_os = "macos")]
mod sandbox_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("pig-sandbox-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).expect("create sandbox workspace");
        std::fs::create_dir_all(&outside).expect("create sandbox outside directory");
        (
            workspace.canonicalize().expect("canonical workspace"),
            outside.canonicalize().expect("canonical outside directory"),
        )
    }

    async fn run(cwd: &Path, state: &SessionToolState, command: &str) -> String {
        let mut tracker = ChangeTracker::default();
        let (out, _) = bash(
            cwd,
            &mut tracker,
            state,
            serde_json::json!({"command": command}),
        )
        .await;
        out
    }

    fn assert_succeeded(output: &str) {
        assert!(output.contains("[exit code: 0]"), "{output}");
    }

    fn assert_failed(output: &str) {
        assert!(!output.contains("[exit code: 0]"), "{output}");
    }

    /// Install what a fresh install runs with: the seeded `[[sandbox.deny]]`
    /// groups and nothing else. The sensitive-file table is configuration now,
    /// so a test that expects `.env` to be denied has to install it.
    fn install_seeded_settings() {
        let mut config = pig_protocol::AppConfig::default();
        config.sandbox.deny = Some(pig_core::config::default_deny_groups());
        pig_core::task::init_sandbox_settings(pig_core::task::SandboxSettings::from_config(
            &config,
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seatbelt_allows_workspace_and_external_system_binaries() {
        let (workspace, _) = fixture("workspace");
        let state = SessionToolState::for_test();
        std::fs::write(workspace.join("inside.txt"), "inside-ok").expect("write inside fixture");

        let read = run(&workspace, &state, "/bin/cat inside.txt").await;
        assert_succeeded(&read);
        assert!(read.contains("inside-ok"), "{read}");

        let write = run(
            &workspace,
            &state,
            "printf changed > inside.txt && /bin/cat inside.txt",
        )
        .await;
        assert_succeeded(&write);
        assert!(write.contains("changed"), "{write}");

        let list = run(&workspace, &state, "/bin/ls").await;
        assert_succeeded(&list);
        assert!(list.contains("inside.txt"), "{list}");
    }

    /// Codex parity (2026-10-19): the filesystem boundary is writes, not reads.
    /// The Seatbelt profile carries a materialized `:root` read entry, so a
    /// command may read anywhere the user can, while writes stay confined to
    /// the workspace (the session-level read/write toggles are gone).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seatbelt_reads_are_full_disk_but_writes_stay_inside() {
        let (workspace, outside) = fixture("outside-reads");
        let state = SessionToolState::for_test();
        std::fs::write(outside.join("outside.txt"), "outside-visible")
            .expect("write outside fixture");

        let read = run(&workspace, &state, "/bin/cat ../outside/outside.txt").await;
        assert_succeeded(&read);
        assert!(read.contains("outside-visible"), "{read}");

        // A deep read far away from the workspace, through the `/` grant.
        let hosts = run(&workspace, &state, "/bin/cat /etc/hosts").await;
        assert_succeeded(&hosts);
        assert!(hosts.contains("localhost"), "{hosts}");

        let denied_write = run(&workspace, &state, "printf denied > ../outside/new.txt").await;
        assert_failed(&denied_write);
        assert!(!outside.join("new.txt").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seatbelt_unknown_executable_and_its_children_keep_the_boundary() {
        let (workspace, outside) = fixture("unknown-executable");
        let state = SessionToolState::for_test();
        // Reads are full-disk now; the boundary an unknown binary (and its
        // children) must keep is writes. The binary writes its argument path.
        std::fs::write(
            workspace.join("unknown.rs"),
            "fn main() {\n    let path = std::env::args().nth(1).unwrap();\n    match std::fs::write(&path, \"binary-wrote\") {\n        Ok(()) => println!(\"wrote {path}\"),\n        Err(error) => {\n            eprintln!(\"{error}\");\n            std::process::exit(1);\n        }\n    }\n}\n",
        )
        .expect("write unknown executable source");
        let compiled = std::process::Command::new("rustc")
            .arg("unknown.rs")
            .arg("-o")
            .arg("unknown-exe")
            .current_dir(&workspace)
            .output()
            .expect("compile unknown executable");
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );

        let inside = run(&workspace, &state, "./unknown-exe inside.txt").await;
        assert_succeeded(&inside);
        assert_eq!(
            std::fs::read_to_string(workspace.join("inside.txt")).unwrap(),
            "binary-wrote"
        );

        let child = run(
            &workspace,
            &state,
            "/bin/sh -c './unknown-exe ../outside/pwned.txt'",
        )
        .await;
        assert_failed(&child);
        assert!(!outside.join("pwned.txt").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seatbelt_sensitive_files_keep_exceptions_and_block_aliases() {
        install_seeded_settings();
        let (workspace, _) = fixture("sensitive");
        let state = SessionToolState::for_test();
        std::fs::write(workspace.join(".env"), "ENV-SECRET").expect("write env fixture");
        std::fs::write(workspace.join(".env.example"), "ENV-TEMPLATE").expect("write env template");
        std::fs::write(workspace.join("id_rsa"), "SSH-SECRET").expect("write key fixture");
        std::fs::write(workspace.join("id_rsa.pub"), "SSH-PUBLIC")
            .expect("write public key fixture");
        std::fs::create_dir(workspace.join(".aws")).expect("create aws directory");
        std::fs::write(workspace.join(".aws/credentials"), "AWS-SECRET")
            .expect("write aws fixture");
        std::os::unix::fs::symlink(".env", workspace.join("env-alias")).expect("create env alias");
        std::os::unix::fs::symlink(".aws", workspace.join("cloud-alias"))
            .expect("create cloud alias");

        for (command, secret) in [
            ("/bin/cat .env", "ENV-SECRET"),
            ("/bin/cat id_rsa", "SSH-SECRET"),
            ("/bin/cat .aws/credentials", "AWS-SECRET"),
            ("/bin/cat env-alias", "ENV-SECRET"),
            ("/bin/cat cloud-alias/credentials", "AWS-SECRET"),
        ] {
            let output = run(&workspace, &state, command).await;
            assert_failed(&output);
            assert!(!output.contains(secret), "{command}: {output}");
        }

        let write = run(&workspace, &state, "printf changed >> .env").await;
        assert_failed(&write);
        assert_eq!(
            std::fs::read_to_string(workspace.join(".env")).unwrap(),
            "ENV-SECRET"
        );

        let env_template = run(&workspace, &state, "/bin/cat .env.example").await;
        assert_succeeded(&env_template);
        assert!(env_template.contains("ENV-TEMPLATE"), "{env_template}");
        let public_key = run(&workspace, &state, "/bin/cat id_rsa.pub").await;
        assert_succeeded(&public_key);
        assert!(public_key.contains("SSH-PUBLIC"), "{public_key}");

        // The table is configuration: deleting a group switches exactly that
        // group off, and every other group keeps applying. The steps run in one
        // test because the settings slot is process-wide and the two
        // configurations must not interleave with each other.
        let mut without_env = pig_protocol::AppConfig::default();
        without_env.sandbox.deny = Some(pig_core::config::default_deny_groups()[1..].to_vec());
        pig_core::task::init_sandbox_settings(pig_core::task::SandboxSettings::from_config(
            &without_env,
        ));
        let readable = run(&workspace, &state, "/bin/cat .env").await;
        let ssh = run(&workspace, &state, "/bin/cat id_rsa").await;
        // Reset the slot before asserting, so a failure cannot leak the
        // weakened table into another test.
        pig_core::task::init_sandbox_settings(pig_core::task::SandboxSettings::new(Vec::new()));

        assert_succeeded(&readable);
        assert!(readable.contains("ENV-SECRET"), "{readable}");
        assert_failed(&ssh);
        assert!(!ssh.contains("SSH-SECRET"), "{ssh}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seatbelt_background_commands_use_the_same_boundary() {
        let (workspace, outside) = fixture("background");
        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();
        let (out, _) = bash(
            &workspace,
            &mut tracker,
            &state,
            serde_json::json!({
                "command": "printf denied > ../outside/background.txt",
                "run_in_background": true
            }),
        )
        .await;
        let task_id = out
            .strip_prefix("Started in the background, task_id: ")
            .and_then(|rest| rest.split('.').next())
            .expect("background task id")
            .to_string();

        let mut finished = false;
        for _ in 0..100 {
            finished = state
                .tasks
                .lock()
                .expect("task registry")
                .iter()
                .any(|entry| {
                    entry.id == task_id
                        && matches!(
                            &entry.status,
                            pig_protocol::TaskStatus::Completed
                                | pig_protocol::TaskStatus::Failed
                                | pig_protocol::TaskStatus::Killed
                        )
                });
            if finished {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert!(finished, "background command did not finish");
        assert!(!outside.join("background.txt").exists());
    }

    /// A `require_escalated` approval is a property of the command, not of the
    /// foreground path. The background spawn used to hard-code
    /// `SandboxOverride::NoOverride`, silently dropping
    /// `EscalatedWithRestrictions`; the approved root write must reach the
    /// background launch while the session's deny rules stay enforced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seatbelt_background_escalation_keeps_its_restrictions() {
        let (workspace, outside) = fixture("background-escalated");
        let state = SessionToolState::for_test();
        *state.sandbox_project.write().expect("project layer") =
            pig_core::config::ProjectSandboxLayer {
                filesystem: Some(std::collections::BTreeMap::from([(
                    "**/.env".to_string(),
                    pig_protocol::SandboxFsPermission::Access(pig_protocol::SandboxFsAccess::Deny),
                )])),
                network_block: None,
            };
        std::fs::write(outside.join(".env"), "SENTINEL").expect("write deny fixture");
        let mut tracker = ChangeTracker::default();
        let (out, is_error, _, _, _) = tool::execute(
            &call(
                "ExecCommand",
                serde_json::json!({
                    "command": "printf escalated > ../outside/background-escalated.txt; printf pwn > ../outside/.env",
                    "run_in_background": true
                }),
            ),
            ToolContext {
                cwd: &workspace,
                tracker: &mut tracker,
                state: &state,
                fs_grant: None,
                sandbox_override: pig_core::task::SandboxOverride::EscalatedWithRestrictions,
                sandbox_profile: Default::default(),
                additional_permissions: Default::default(),
            },
        )
        .await;
        assert!(!is_error, "{out}");
        let task_id = out
            .strip_prefix("Started in the background, task_id: ")
            .and_then(|rest| rest.split('.').next())
            .expect("background task id")
            .to_string();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let finished = state
                .tasks
                .lock()
                .expect("task registry")
                .iter()
                .any(|entry| {
                    entry.id == task_id
                        && matches!(
                            &entry.status,
                            pig_protocol::TaskStatus::Completed
                                | pig_protocol::TaskStatus::Failed
                                | pig_protocol::TaskStatus::Killed
                        )
                });
            if finished {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "background escalation did not finish"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            std::fs::read_to_string(outside.join("background-escalated.txt"))
                .unwrap_or_default()
                .trim(),
            "escalated",
            "the approved root write must apply to the background launch"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join(".env")).unwrap(),
            "SENTINEL",
            "the deny rules must stay enforced under the escalation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seatbelt_real_toolchains_still_run() {
        let (workspace, _) = fixture("toolchains");
        let state = SessionToolState::for_test();
        std::fs::create_dir(workspace.join("src")).expect("create source directory");
        std::fs::write(
            workspace.join("Cargo.toml"),
            // The fixture lives under this repo's `target/tmp`, and with
            // full-disk reads cargo now finds the outer workspace root — an
            // empty `[workspace]` makes the fixture its own root.
            "[package]\nname = \"sandbox-smoke\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .expect("write smoke manifest");
        std::fs::write(workspace.join("src/lib.rs"), "pub fn smoke() {}\n")
            .expect("write smoke source");

        let git = run(&workspace, &state, "git --version").await;
        assert_succeeded(&git);
        let cargo = run(
            &workspace,
            &state,
            "cargo metadata --no-deps --offline --format-version 1",
        )
        .await;
        assert_succeeded(&cargo);
        assert!(cargo.contains("sandbox-smoke"), "{cargo}");
    }

    /// `USERPROFILE` is process-wide; the credential tests redirect it. An async
    /// semaphore: the guard is held across `await`.
    async fn credential_env_lock() -> tokio::sync::OwnedSemaphorePermit {
        static LOCK: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)))
            .clone()
            .acquire_owned()
            .await
            .expect("credential env lock")
    }

    /// The profile's credential directories are denied whole in the Seatbelt
    /// profile: not listable, not readable. An approved single file reads
    /// through the scoped-rule exception while its siblings stay denied — and
    /// a name-gloomed private key reads only when IT is the approved file (the
    /// exception lands after the group deny, so the named approval wins).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seatbelt_credential_store_denied_whole_but_one_file_approvable() {
        let _env = credential_env_lock().await;
        let (workspace, outside) = fixture("credential-store");
        let profile = outside.parent().expect("fixture root").join("profile");
        let store = profile.join(".ssh");
        std::fs::create_dir_all(&store).expect("create fixture store");
        std::fs::write(store.join("config"), "HOST-CONFIG").unwrap();
        std::fs::write(store.join("known_hosts"), "KNOWN-HOSTS").unwrap();
        std::fs::write(store.join("id_rsa"), "PRIVATE-KEY").unwrap();
        // SAFETY: serialized on `credential_env_lock`.
        let previous = std::env::var_os("USERPROFILE");
        unsafe { std::env::set_var("USERPROFILE", &profile) };
        let state = SessionToolState::for_test();

        // No grant: the store cannot even be listed, and no content reads.
        let ls = run(&workspace, &state, &format!("ls {}", store.display())).await;
        assert_failed(&ls);
        assert!(!ls.contains("known_hosts"), "{ls}");
        let config = store.join("config");
        let cat = run(
            &workspace,
            &state,
            &format!("/bin/cat {}", config.display()),
        )
        .await;
        assert_failed(&cat);
        assert!(!cat.contains("HOST-CONFIG"), "{cat}");

        let known_hosts = store.join("known_hosts");
        async fn run_with_read(
            workspace: &Path,
            state: &SessionToolState,
            approved: &Path,
            command: String,
        ) -> (String, bool) {
            let mut tracker = ChangeTracker::default();
            let (out, is_error, _, _, _) = tool::execute(
                &call("ExecCommand", serde_json::json!({"command": command})),
                ToolContext {
                    cwd: workspace,
                    tracker: &mut tracker,
                    state,
                    fs_grant: None,
                    sandbox_override: Default::default(),
                    sandbox_profile: pig_core::task::SandboxProfile::WorkspaceWrite,
                    additional_permissions: pig_core::task::AdditionalPermissions::read_roots(
                        vec![approved.to_path_buf()],
                    ),
                },
            )
            .await;
            (out, is_error)
        }
        // An approved single file reads through the scoped-rule exception…
        let approved = run_with_read(
            &workspace,
            &state,
            &known_hosts,
            format!("/bin/cat {}", known_hosts.display()),
        )
        .await;
        assert_succeeded(&approved.0);
        assert!(approved.0.contains("KNOWN-HOSTS"), "{}", approved.0);
        // …while a sibling stays denied under the same overlay…
        let sibling = run_with_read(
            &workspace,
            &state,
            &known_hosts,
            format!("/bin/cat {}", config.display()),
        )
        .await;
        assert_failed(&sibling.0);
        assert!(!sibling.0.contains("HOST-CONFIG"), "{}", sibling.0);
        // …and the private key stays denied until IT is the approved file.
        let id_rsa = store.join("id_rsa");
        let key_denied = run_with_read(
            &workspace,
            &state,
            &known_hosts,
            format!("/bin/cat {}", id_rsa.display()),
        )
        .await;
        assert!(!key_denied.0.contains("PRIVATE-KEY"), "{}", key_denied.0);
        let key_approved = run_with_read(
            &workspace,
            &state,
            &id_rsa,
            format!("/bin/cat {}", id_rsa.display()),
        )
        .await;
        assert!(key_approved.0.contains("PRIVATE-KEY"), "{}", key_approved.0);

        // A listing approval (the grant names the store itself): names visible,
        // contents still denied.
        let listing = run_with_read(
            &workspace,
            &state,
            &store,
            format!("ls {} && /bin/cat {}", store.display(), config.display()),
        )
        .await;
        assert!(listing.0.contains("known_hosts"), "{}", listing.0);
        assert!(!listing.0.contains("HOST-CONFIG"), "{}", listing.0);
        let long_listing = run_with_read(
            &workspace,
            &state,
            &store,
            format!("ls -la {}", store.display()),
        )
        .await;
        assert!(long_listing.0.contains("known_hosts"), "{}", long_listing.0);

        if let Some(previous) = previous {
            unsafe { std::env::set_var("USERPROFILE", previous) };
        } else {
            unsafe { std::env::remove_var("USERPROFILE") };
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_sandbox_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The sandbox settings (`[[sandbox.deny]]` groups, `deny_read`) live in a
    /// process-wide slot, because production has
    /// exactly one config per process. These tests run in parallel by default,
    /// so every test in this module serializes on one permit: a test that
    /// installs a weakened rule table must not be observed by another test
    /// mid-flight. An async semaphore rather than a `Mutex` because the guard
    /// is held across `await` (Clippy's `await_holding_lock` is right about that).
    async fn sandbox_lock() -> tokio::sync::OwnedSemaphorePermit {
        static LOCK: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)))
            .clone()
            .acquire_owned()
            .await
            .expect("sandbox test lock")
    }

    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        // Unique per run: a previous failed sandbox run can leave DENY ACEs on a
        // fixture directory, and reusing the name would then make the *test*
        // unable to read its own files.
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!(
                "pig-sandbox-{name}-{}-{unique}",
                std::process::id()
            ));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).expect("create sandbox workspace");
        std::fs::create_dir_all(&outside).expect("create sandbox outside directory");
        // Inheritance is left intact on purpose: the fixture models an
        // ordinary directory outside the workspace, which reads are allowed to
        // reach (full-disk read) while writes stay confined to the workspace.
        (
            workspace.canonicalize().expect("canonical workspace"),
            outside.canonicalize().expect("canonical outside directory"),
        )
    }

    /// Install what a fresh install runs with: the seeded `[[sandbox.deny]]`
    /// groups and nothing else. The sensitive-file table is configuration now,
    /// so a test that expects `.env` to be denied has to install it.
    fn install_seeded_settings() {
        let mut config = pig_protocol::AppConfig::default();
        config.sandbox.deny = Some(pig_core::config::default_deny_groups());
        pig_core::task::init_sandbox_settings(pig_core::task::SandboxSettings::from_config(
            &config,
        ));
    }

    /// Empty the process-wide settings slot (every test here mutates it).
    fn reset_sandbox_settings() {
        pig_core::task::init_sandbox_settings(pig_core::task::SandboxSettings::new(Vec::new()));
    }

    /// The model shell on Windows is PowerShell (falling back to cmd): Git Bash
    /// cannot start inside the PSEC AppContainer token, so the tests must speak
    /// the detected dialect.
    fn shell_kind() -> &'static str {
        match pig_core::task::windows_sandbox_shell() {
            pig_core::task::WindowsSandboxShell::PowerShell(_) => "powershell",
            pig_core::task::WindowsSandboxShell::Cmd => "cmd",
        }
    }

    fn cat(path: &str) -> String {
        if shell_kind() == "cmd" {
            format!("type {path}")
        } else {
            format!("cat {path}")
        }
    }

    fn write_file(path: &str, text: &str) -> String {
        match shell_kind() {
            "powershell" => {
                format!("[System.IO.File]::WriteAllText('{path}', '{text}')")
            }
            _ => format!("echo {text}> {path}"),
        }
    }

    fn append_file(path: &str, text: &str) -> String {
        match shell_kind() {
            "powershell" => {
                format!("[System.IO.File]::AppendAllText('{path}', '{text}')")
            }
            _ => format!("echo {text}>> {path}"),
        }
    }

    async fn run(cwd: &Path, state: &SessionToolState, command: &str) -> String {
        run_with_profile(
            cwd,
            state,
            command,
            pig_core::task::SandboxProfile::WorkspaceWrite,
        )
        .await
    }

    /// Same call with an explicit filesystem profile (plan mode uses `ReadOnly`).
    async fn run_with_profile(
        cwd: &Path,
        state: &SessionToolState,
        command: &str,
        profile: pig_core::task::SandboxProfile,
    ) -> String {
        let mut tracker = ChangeTracker::default();
        let (out, _, _, _, _) = tool::execute(
            &call("ExecCommand", serde_json::json!({"command": command})),
            ToolContext {
                cwd,
                tracker: &mut tracker,
                state,
                fs_grant: None,
                sandbox_override: Default::default(),
                sandbox_profile: profile,
                additional_permissions: Default::default(),
            },
        )
        .await;
        out
    }

    /// Same call with an explicit sandbox override/profile (approval flows).
    async fn run_with_override(
        cwd: &Path,
        state: &SessionToolState,
        command: &str,
        sandbox_override: pig_core::task::SandboxOverride,
        profile: pig_core::task::SandboxProfile,
    ) -> String {
        let mut tracker = ChangeTracker::default();
        let (out, _, _, _, _) = tool::execute(
            &call("ExecCommand", serde_json::json!({"command": command})),
            ToolContext {
                cwd,
                tracker: &mut tracker,
                state,
                fs_grant: None,
                sandbox_override,
                sandbox_profile: profile,
                additional_permissions: Default::default(),
            },
        )
        .await;
        out
    }

    fn assert_succeeded(output: &str) {
        assert!(output.contains("[exit code: 0]"), "{output}");
    }

    fn assert_failed(output: &str) {
        assert!(!output.contains("[exit code: 0]"), "{output}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_allows_workspace_and_system_binaries() {
        let _guard = sandbox_lock().await;
        let (workspace, _) = fixture("workspace");
        let state = SessionToolState::for_test();
        std::fs::write(workspace.join("inside.txt"), "inside-ok").expect("write inside fixture");

        let read = run(&workspace, &state, &cat("inside.txt")).await;
        assert_succeeded(&read);
        assert!(read.contains("inside-ok"), "{read}");

        let write = run(&workspace, &state, &write_file("changed.txt", "changed")).await;
        assert_succeeded(&write);
        assert_eq!(
            std::fs::read_to_string(workspace.join("changed.txt"))
                .expect("read back")
                .trim(),
            "changed"
        );

        // A real toolchain binary from outside the workspace must still run.
        let git = run(&workspace, &state, "git --version").await;
        assert_succeeded(&git);
        assert!(git.to_lowercase().contains("git version"), "{git}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_reads_are_full_disk_but_writes_stay_inside() {
        let _guard = sandbox_lock().await;
        let (workspace, outside) = fixture("outside");
        let state = SessionToolState::for_test();
        std::fs::write(outside.join("outside.txt"), "outside-visible")
            .expect("write outside fixture");

        // Full-disk read (Codex's `workspace-write` shape): the sandbox may
        // inspect anything the user can, so this succeeds.
        let read = run(&workspace, &state, &cat("../outside/outside.txt")).await;
        assert_succeeded(&read);
        assert!(read.contains("outside-visible"), "{read}");

        // Writes are the boundary: still confined to the workspace.
        let denied_write = run(
            &workspace,
            &state,
            &write_file("../outside/new.txt", "denied"),
        )
        .await;
        assert_failed(&denied_write);
        assert!(!outside.join("new.txt").exists());

        // A volume-root grant is recursive on this MXC build, not a
        // non-recursive mount of the root's immediate children. Pin that so an
        // MXC change making it shallow cannot silently narrow full-disk read
        // (Codex expands volume roots for exactly this reason).
        // The fixture is canonicalized, and on Windows that makes its
        // volume-root ancestor a `\\?\C:\` verbatim path. Slash-normalizing it
        // yields `//?/C:/...`, which PowerShell's Get-Content cannot resolve,
        // so drop the prefix and keep native separators.
        let volume_root = outside.ancestors().last().expect("filesystem root");
        let volume_root = volume_root.to_string_lossy();
        let volume_root = volume_root.strip_prefix(r"\\?\").unwrap_or(&volume_root);
        let deep = Path::new(volume_root)
            .join("Windows")
            .join("System32")
            .join("drivers")
            .join("etc")
            .join("hosts");
        if deep.is_file() {
            let path = deep.display().to_string();
            let read = run(&workspace, &state, &cat(&path)).await;
            assert_succeeded(&read);
            assert!(read.contains("localhost") || read.len() > 10, "{read}");
        }
    }

    /// A junction inside the workspace resolves to its outside target, so the
    /// object-based write grant still confines writes: the read follows the
    /// alias (full-disk read), the write is denied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_junction_alias_keeps_the_write_boundary() {
        let _guard = sandbox_lock().await;
        let (workspace, outside) = fixture("junction");
        let state = SessionToolState::for_test();
        std::fs::write(outside.join("outside.txt"), "OUTSIDE-READ").expect("write outside fixture");
        common::junction(&workspace.join("alias"), &outside);

        let read = run(&workspace, &state, &cat(r"alias\outside.txt")).await;
        assert_succeeded(&read);
        assert!(read.contains("OUTSIDE-READ"), "{read}");

        let write = run(
            &workspace,
            &state,
            &write_file(r"alias\outside.txt", "clobber"),
        )
        .await;
        assert_failed(&write);
        assert_eq!(
            std::fs::read_to_string(outside.join("outside.txt")).expect("host read"),
            "OUTSIDE-READ",
            "a junction alias must not carry a workspace write outside"
        );
    }

    /// The read-only profile is enforced by the kernel, not by the tool layer:
    /// a plan-mode command may still read (and run toolchain binaries) but
    /// every write fails, including one inside the workspace.
    ///
    /// The workspace is written **first** under the normal profile, so the
    /// interesting case is covered: a file the sandbox itself created while
    /// writes were allowed must be denied once the profile turns read-only.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_read_only_profile_denies_every_write() {
        let _guard = sandbox_lock().await;
        let (workspace, outside) = fixture("read-only");
        let state = SessionToolState::for_test();
        std::fs::write(workspace.join("input.txt"), "READ-ONLY-OK").expect("write fixture");

        // Establish the ordinary write grant first, and create a file *through
        // the sandbox* so it carries the inherited allow ACE a real workspace
        // file has — that is exactly the case a naive "don't grant it this
        // time" profile still lets through.
        let warmup = run(&workspace, &state, &write_file("warmup.txt", "ok-warm")).await;
        assert_succeeded(&warmup);
        assert_eq!(
            std::fs::read_to_string(workspace.join("warmup.txt"))
                .expect("host read")
                .trim(),
            "ok-warm"
        );

        // Reading still works (full-disk read is unchanged).
        let read = run_with_profile(
            &workspace,
            &state,
            &cat("input.txt"),
            pig_core::task::SandboxProfile::ReadOnly,
        )
        .await;
        assert_succeeded(&read);
        assert!(read.contains("READ-ONLY-OK"), "{read}");

        // A toolchain binary still runs.
        let git = run_with_profile(
            &workspace,
            &state,
            "git --version",
            pig_core::task::SandboxProfile::ReadOnly,
        )
        .await;
        assert_succeeded(&git);

        // Every write target is denied: a new file, an existing file, and a
        // path outside the workspace.
        let cases = [
            write_file("created.txt", "nope"),
            append_file("input.txt", "-nope"),
            // A file the sandbox itself created while writes were allowed.
            append_file("warmup.txt", "-nope"),
            write_file(
                &format!(
                    "{}/created.txt",
                    outside.to_string_lossy().replace('\\', "/")
                ),
                "nope",
            ),
        ];
        for command in cases {
            let output = run_with_profile(
                &workspace,
                &state,
                &command,
                pig_core::task::SandboxProfile::ReadOnly,
            )
            .await;
            assert_failed(&output);
        }
        assert!(!workspace.join("created.txt").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.join("input.txt")).expect("host read"),
            "READ-ONLY-OK"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("warmup.txt"))
                .expect("host read")
                .trim(),
            "ok-warm",
            "a file created while writes were allowed must not change"
        );
        assert!(!outside.join("created.txt").exists());

        // A workspace-write launch afterwards is writable again: the profile is
        // per command, not a persistent downgrade.
        let write = run(&workspace, &state, &write_file("after.txt", "ok")).await;
        assert_succeeded(&write);
        assert!(workspace.join("after.txt").exists());
    }

    /// `with_additional_permissions` widens the sandbox for one command: the
    /// approved root becomes writable, everything else (sensitive files, the
    /// workspace boundary for other commands) is unaffected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_additional_write_root_is_granted_for_one_command() {
        let _guard = sandbox_lock().await;
        let (workspace, outside) = fixture("escalation");
        let state = SessionToolState::for_test();
        // The sensitive-file table is the seeded `[[sandbox.deny]]` config, not
        // code: install exactly what a fresh install runs with.
        install_seeded_settings();

        let outside_path = outside.to_string_lossy().replace('\\', "/");
        let target = format!("{outside_path}/granted.txt");

        // Without the grant the outside write is denied.
        let denied = run(&workspace, &state, &write_file(&target, "denied")).await;
        assert_failed(&denied);
        assert!(!outside.join("granted.txt").exists());

        // With the root granted, the same write succeeds.
        let granted = tool::execute(
            &call(
                "ExecCommand",
                serde_json::json!({
                    "command": write_file(&target, "granted"),
                    "sandbox_permissions": "with_additional_permissions",
                    "additional_permissions": {
                        "file_system": { "write": [outside.to_string_lossy()] }
                    },
                    "justification": "writes a generated report outside the workspace"
                }),
            ),
            ToolContext {
                cwd: &workspace,
                tracker: &mut ChangeTracker::default(),
                state: &state,
                fs_grant: None,
                sandbox_override: Default::default(),
                sandbox_profile: pig_core::task::SandboxProfile::WorkspaceWrite,
                additional_permissions: pig_core::task::AdditionalPermissions::write_roots(vec![
                    outside.clone(),
                ]),
            },
        )
        .await;
        assert_succeeded(&granted.0);
        assert_eq!(
            std::fs::read_to_string(outside.join("granted.txt"))
                .expect("read granted file")
                .trim(),
            "granted"
        );

        // The approval widens writes, not reads: a sensitive file in the
        // approved directory stays denied.
        std::fs::write(outside.join(".env"), "OUTSIDE-SECRET").expect("write secret fixture");
        let secret = tool::execute(
            &call(
                "ExecCommand",
                serde_json::json!({
                    "command": cat(&format!("{outside_path}/.env")),
                    "sandbox_permissions": "with_additional_permissions",
                    "additional_permissions": {
                        "file_system": { "write": [outside.to_string_lossy()] }
                    }
                }),
            ),
            ToolContext {
                cwd: &workspace,
                tracker: &mut ChangeTracker::default(),
                state: &state,
                fs_grant: None,
                sandbox_override: Default::default(),
                sandbox_profile: pig_core::task::SandboxProfile::WorkspaceWrite,
                additional_permissions: pig_core::task::AdditionalPermissions::write_roots(vec![
                    outside.clone(),
                ]),
            },
        )
        .await;
        assert!(!secret.0.contains("OUTSIDE-SECRET"), "{}", secret.0);

        // The grant is per command: the next one is confined again.
        let after = run(
            &workspace,
            &state,
            &write_file(&format!("{outside_path}/after.txt"), "x"),
        )
        .await;
        assert_failed(&after);
        assert!(!outside.join("after.txt").exists());
        reset_sandbox_settings();
    }

    /// Codex's `EscalatedSandboxWithRestrictions`: the approved command gets a
    /// root write, but the deny list stays enforced. This is the Windows path
    /// `require_escalated` takes when read denials are active.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_escalated_restrictions_widen_writes_but_keep_denies() {
        let _guard = sandbox_lock().await;
        let (workspace, outside) = fixture("escalated-restrictions");
        let state = SessionToolState::for_test();
        install_seeded_settings();
        std::fs::write(workspace.join(".env"), "ENV-SECRET").expect("write env fixture");

        // Relative to the workspace: a `\\?\` verbatim path is a different
        // native spelling from the policy's root grant.
        let target = "../outside/granted.txt".to_string();
        let granted = run_with_override(
            &workspace,
            &state,
            &write_file(&target, "granted"),
            pig_core::task::SandboxOverride::EscalatedWithRestrictions,
            pig_core::task::SandboxProfile::WorkspaceWrite,
        )
        .await;
        assert_succeeded(&granted);
        assert_eq!(
            std::fs::read_to_string(outside.join("granted.txt")).expect("outside file"),
            "granted"
        );

        let secret = run_with_override(
            &workspace,
            &state,
            &cat(".env"),
            pig_core::task::SandboxOverride::EscalatedWithRestrictions,
            pig_core::task::SandboxProfile::WorkspaceWrite,
        )
        .await;
        assert_failed(&secret);
        assert!(!secret.contains("ENV-SECRET"), "{secret}");

        reset_sandbox_settings();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_sensitive_files_keep_exceptions() {
        let _guard = sandbox_lock().await;
        let (workspace, _) = fixture("sensitive");
        let state = SessionToolState::for_test();
        // The sensitive-file table is the seeded `[[sandbox.deny]]` config, not
        // code: install exactly what a fresh install runs with.
        install_seeded_settings();
        std::fs::write(workspace.join(".env"), "ENV-SECRET").expect("write env fixture");
        std::fs::write(workspace.join(".env.example"), "ENV-TEMPLATE").expect("write env template");
        std::fs::write(workspace.join("id_rsa"), "SSH-SECRET").expect("write key fixture");
        std::fs::write(workspace.join("id_rsa.pub"), "SSH-PUBLIC")
            .expect("write public key fixture");
        std::fs::create_dir(workspace.join(".aws")).expect("create aws directory");
        std::fs::write(workspace.join(".aws").join("credentials"), "AWS-SECRET")
            .expect("write aws fixture");

        for (path, secret) in [
            (".env", "ENV-SECRET"),
            ("id_rsa", "SSH-SECRET"),
            (".aws/credentials", "AWS-SECRET"),
        ] {
            let output = run(&workspace, &state, &cat(path)).await;
            assert_failed(&output);
            assert!(!output.contains(secret), "{path}: {output}");
        }

        let write = run(&workspace, &state, &append_file(".env", "changed")).await;
        assert_failed(&write);
        assert_eq!(
            std::fs::read_to_string(workspace.join(".env")).expect("read env"),
            "ENV-SECRET"
        );

        let template = run(&workspace, &state, &cat(".env.example")).await;
        assert_succeeded(&template);
        assert!(template.contains("ENV-TEMPLATE"), "{template}");
        let public_key = run(&workspace, &state, &cat("id_rsa.pub")).await;
        assert_succeeded(&public_key);
        assert!(public_key.contains("SSH-PUBLIC"), "{public_key}");

        reset_sandbox_settings();
    }

    /// A configured `[sandbox] deny_read` entry becomes a kernel denial: both
    /// a literal path and a glob are exercised, and the file stays readable
    /// outside the sandbox (so the denial is the sandbox, not filesystem
    /// permissions).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_applies_configured_deny_read_rules() {
        let _guard = sandbox_lock().await;
        let (workspace, _) = fixture("deny-read-config");
        let state = SessionToolState::for_test();
        let vault = workspace.join("vault");
        std::fs::create_dir_all(&vault).expect("create vault");
        std::fs::write(vault.join("token.txt"), "TOKEN-SECRET").expect("write literal target");
        std::fs::create_dir_all(workspace.join("certs")).expect("create certs");
        std::fs::write(workspace.join("certs").join("site.pem"), "PEM-SECRET")
            .expect("write glob target");
        std::fs::write(workspace.join("certs").join("readme.md"), "PUBLIC")
            .expect("write public file");

        pig_core::task::init_sandbox_settings(pig_core::task::SandboxSettings::new(vec![
            "vault".to_string(),
            "**/*.pem".to_string(),
        ]));
        let literal = run(&workspace, &state, &cat("vault/token.txt")).await;
        assert_failed(&literal);
        assert!(!literal.contains("TOKEN-SECRET"), "{literal}");

        let glob = run(&workspace, &state, &cat("certs/site.pem")).await;
        assert_failed(&glob);
        assert!(!glob.contains("PEM-SECRET"), "{glob}");

        // The rules are targeted: a sibling file in the same directory and a
        // file outside the sandbox work normally.
        let public = run(&workspace, &state, &cat("certs/readme.md")).await;
        assert_succeeded(&public);
        assert!(public.contains("PUBLIC"), "{public}");
        assert_eq!(
            std::fs::read_to_string(vault.join("token.txt")).expect("host read"),
            "TOKEN-SECRET",
            "the file itself must stay intact"
        );

        // Clearing the rules takes effect on the next launch: the policy is
        // rebuilt per command.
        reset_sandbox_settings();
        let allowed = run(&workspace, &state, &cat("vault/token.txt")).await;
        assert_succeeded(&allowed);
        assert!(allowed.contains("TOKEN-SECRET"), "{allowed}");
    }

    /// `[sandbox.filesystem]`: a denial with a deeper read grant keeps exactly
    /// that one file reachable. This is the combination Codex's config can
    /// express and our rule model could already represent — the entry table is
    /// what makes it configurable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_filesystem_entries_except_one_file() {
        let _guard = sandbox_lock().await;
        let (workspace, _) = fixture("filesystem-entries");
        let state = SessionToolState::for_test();
        let vault = workspace.join("vault");
        std::fs::create_dir_all(&vault).expect("create vault");
        std::fs::write(vault.join("token.txt"), "VAULT-SECRET").expect("write secret");
        std::fs::write(vault.join("public.txt"), "VAULT-PUBLIC").expect("write public file");

        let mut config = pig_protocol::AppConfig::default();
        config.sandbox.filesystem.insert(
            vault.display().to_string(),
            pig_protocol::SandboxFsPermission::Access(pig_protocol::SandboxFsAccess::Deny),
        );
        config.sandbox.filesystem.insert(
            vault.join("public.txt").display().to_string(),
            pig_protocol::SandboxFsPermission::Access(pig_protocol::SandboxFsAccess::Read),
        );
        pig_core::task::init_sandbox_settings(pig_core::task::SandboxSettings::from_config(
            &config,
        ));

        let denied = run(&workspace, &state, &cat("vault/token.txt")).await;
        let excepted = run(&workspace, &state, &cat("vault/public.txt")).await;

        // Reset the process-wide slot before asserting.
        reset_sandbox_settings();

        assert_failed(&denied);
        assert!(!denied.contains("VAULT-SECRET"), "{denied}");
        assert_succeeded(&excepted);
        assert!(excepted.contains("VAULT-PUBLIC"), "{excepted}");
    }

    /// A **file** inside a protected credential directory can be approved and
    /// then written, while everything next to it stays denied. This is the
    /// usability half of the credential-directory rule; the directory itself
    /// stays unapprovable (`credentials_are_not_approvable_as_a_directory` in
    /// the gate's unit tests and the refusal path below).
    ///
    /// The profile is redirected to a fixture, so the test never touches the
    /// real one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_approved_credential_file_is_writable() {
        let _guard = sandbox_lock().await;
        let (workspace, outside) = fixture("credential-file");
        let state = SessionToolState::for_test();
        let profile = outside.join("profile");
        std::fs::create_dir_all(profile.join(".ssh")).expect("create fake profile");
        let known_hosts = profile.join(".ssh").join("known_hosts");
        std::fs::write(&known_hosts, "OLD-ENTRY").expect("write fixture");
        std::fs::write(profile.join(".ssh").join("id_rsa"), "PRIVATE-KEY").expect("write sibling");
        // SAFETY: the sandbox tests serialize on `sandbox_lock`; the policy
        // builder is the only reader of this variable in the command path.
        let previous = std::env::var_os("USERPROFILE");
        unsafe { std::env::set_var("USERPROFILE", &profile) };

        let path = known_hosts
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let sibling = profile
            .join(".ssh")
            .join("id_rsa")
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");

        // No grant: the directory is denied whole.
        let denied = run(&workspace, &state, &cat(&path)).await;

        // The approved file: writable for this one command.
        let granted = tool::execute(
            &call(
                "ExecCommand",
                serde_json::json!({
                    "command": write_file(&path, "NEW-ENTRY"),
                    "sandbox_permissions": "with_additional_permissions",
                    "additional_permissions": {
                        "file_system": { "write": [known_hosts.to_string_lossy()] }
                    },
                    "justification": "updates one known_hosts entry"
                }),
            ),
            ToolContext {
                cwd: &workspace,
                tracker: &mut ChangeTracker::default(),
                state: &state,
                fs_grant: None,
                sandbox_override: Default::default(),
                sandbox_profile: pig_core::task::SandboxProfile::WorkspaceWrite,
                additional_permissions: pig_core::task::AdditionalPermissions::write_roots(vec![
                    known_hosts.clone(),
                ]),
            },
        )
        .await;
        let after = std::fs::read_to_string(&known_hosts).unwrap_or_default();
        // The sibling is still denied in the same launch.
        let sibling_read = tool::execute(
            &call(
                "ExecCommand",
                serde_json::json!({
                    "command": cat(&sibling),
                    "sandbox_permissions": "with_additional_permissions",
                    "additional_permissions": {
                        "file_system": { "write": [known_hosts.to_string_lossy()] }
                    }
                }),
            ),
            ToolContext {
                cwd: &workspace,
                tracker: &mut ChangeTracker::default(),
                state: &state,
                fs_grant: None,
                sandbox_override: Default::default(),
                sandbox_profile: pig_core::task::SandboxProfile::WorkspaceWrite,
                additional_permissions: pig_core::task::AdditionalPermissions::write_roots(vec![
                    known_hosts.clone(),
                ]),
            },
        )
        .await;
        if let Some(previous) = previous {
            unsafe { std::env::set_var("USERPROFILE", previous) };
        } else {
            unsafe { std::env::remove_var("USERPROFILE") };
        }

        assert_failed(&denied);
        assert!(!denied.contains("OLD-ENTRY"), "{denied}");
        assert_succeeded(&granted.0);
        assert_eq!(after.trim(), "NEW-ENTRY");
        assert!(
            !sibling_read.0.contains("PRIVATE-KEY"),
            "the granted file must not open its siblings: {}",
            sibling_read.0
        );
    }

    /// A credential directory is opaque until the user approves a file in it:
    /// without an approval neither the listing nor any file is reachable, and
    /// with one approved file that file reads.
    ///
    /// Listing is not a right of its own — reaching a file means traversing its
    /// parents, and Windows grants enumeration together with traverse, so the
    /// directory becomes listable exactly when an approval opens a path through
    /// it. Pinned against the real kernel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_credential_directory_needs_an_approval_to_list() {
        let _guard = sandbox_lock().await;
        let (workspace, outside) = fixture("credential-listing");
        let state = SessionToolState::for_test();
        let profile = outside.join("profile");
        std::fs::create_dir_all(profile.join(".ssh").join("keys")).expect("create");
        let known_hosts = profile.join(".ssh").join("known_hosts");
        std::fs::write(&known_hosts, "KNOWN-HOSTS-ENTRY").expect("write");
        let id_rsa = profile.join(".ssh").join("id_rsa");
        std::fs::write(&id_rsa, "PRIVATE-KEY").expect("write");
        let previous = std::env::var_os("USERPROFILE");
        // SAFETY: the sandbox tests serialize on `sandbox_lock`; the policy
        // builder is the only reader of this variable in the command path.
        unsafe { std::env::set_var("USERPROFILE", &profile) };

        let path_of = |path: &std::path::Path| {
            path.to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/")
        };
        let ssh_path = path_of(&profile.join(".ssh"));
        let listing = format!("[System.IO.Directory]::GetFileSystemEntries('{ssh_path}')");
        let read_key = format!("[System.IO.File]::ReadAllText('{}')", path_of(&id_rsa));
        let read_known = format!("[System.IO.File]::ReadAllText('{}')", path_of(&known_hosts));

        // No approval: the directory cannot even be listed, and no file reads.
        let plain_listing = run(&workspace, &state, &listing).await;
        let plain_key = run(&workspace, &state, &read_key).await;

        // One approved file: it reads, and the traversal that made that possible
        // also makes the directory listable.
        let known_hosts_owned = known_hosts.clone();
        let widened = |command: String| {
            let state = state.clone();
            let workspace = workspace.clone();
            let grant = known_hosts_owned.clone();
            async move {
                tool::execute(
                    &call(
                        "ExecCommand",
                        serde_json::json!({
                            "command": command,
                            "sandbox_permissions": "with_additional_permissions",
                            "additional_permissions": {
                                "file_system": { "write": [grant.to_string_lossy()] }
                            }
                        }),
                    ),
                    ToolContext {
                        cwd: &workspace,
                        tracker: &mut ChangeTracker::default(),
                        state: &state,
                        fs_grant: None,
                        sandbox_override: Default::default(),
                        sandbox_profile: pig_core::task::SandboxProfile::WorkspaceWrite,
                        additional_permissions: pig_core::task::AdditionalPermissions::write_roots(
                            vec![grant.clone()],
                        ),
                    },
                )
                .await
                .0
            }
        };
        let granted_known = widened(read_known).await;
        let granted_listing = widened(listing).await;
        let granted_key = widened(read_key).await;
        if let Some(previous) = previous {
            unsafe { std::env::set_var("USERPROFILE", previous) };
        } else {
            unsafe { std::env::remove_var("USERPROFILE") };
        }

        assert_failed(&plain_listing);
        assert!(
            !plain_listing.contains("known_hosts"),
            "the directory must not be listable before an approval: {plain_listing}"
        );
        assert_failed(&plain_key);
        assert!(!plain_key.contains("PRIVATE-KEY"), "{plain_key}");

        assert_succeeded(&granted_known);
        assert!(
            granted_known.contains("KNOWN-HOSTS-ENTRY"),
            "{granted_known}"
        );
        assert_succeeded(&granted_listing);
        assert!(granted_listing.contains("known_hosts"), "{granted_listing}");
        // The listing came with traversal, not with access to the siblings.
        assert_failed(&granted_key);
        assert!(!granted_key.contains("PRIVATE-KEY"), "{granted_key}");
    }

    /// The kernel denies things the policy's own text allows. This is the gap a
    /// pure in-process `ResolvedPolicy` check would miss, so it is pinned: every
    /// path below is inside the `:root` read grant, and the native launcher still
    /// refuses it.
    ///
    /// Cause: the plan hands `:root`'s materialized volume roots to the launcher
    /// through the well-known-SID read path (`platform_read_paths`) rather than
    /// as hand-made grants, and a profile root is not granted at all. The
    /// resulting ACE set covers paths that carry ordinary ACLs, not everything
    /// the policy names — so "readable per policy" is not "readable per kernel".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_denies_paths_the_policy_text_allows() {
        let _guard = sandbox_lock().await;
        let (workspace, _) = fixture("policy-vs-kernel");
        let state = SessionToolState::for_test();

        let probe = |path: &str| {
            format!(
                "try {{ [System.IO.File]::ReadAllText('{path}') | Out-Null; \
                 Write-Output 'READ-OK' }} catch {{ Write-Output 'READ-DENIED' }}"
            )
        };
        // `hosts` is inside `C:\Windows`, which the launcher treats specially;
        // it is readable because the file itself carries an ordinary ACL.
        let system_file = run(
            &workspace,
            &state,
            &probe("C:/Windows/System32/drivers/etc/hosts"),
        )
        .await;
        // `C:\Windows\Temp` sits under the same root and is refused.
        let system_temp = run(&workspace, &state, &probe("C:/Windows/Temp")).await;
        // A drive root itself is never a file, so probe a file directly under a
        // profile directory: it is inside the full-disk read grant, and refused.
        let profile_root = std::env::var("USERPROFILE").unwrap_or_default();
        let profile_file = run(&workspace, &state, &probe(&profile_root)).await;

        assert!(system_file.contains("READ-OK"), "{system_file}");
        assert!(
            system_temp.contains("READ-DENIED"),
            "the kernel denies a path inside the read grant: {system_temp}"
        );
        assert!(
            profile_file.contains("READ-DENIED"),
            "the kernel denies the profile root: {profile_file}"
        );
    }

    /// An approved **listing** of a credential directory: names become visible,
    /// contents do not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_credential_directory_listing_can_be_approved() {
        let _guard = sandbox_lock().await;
        let (workspace, outside) = fixture("cred-listing");
        let state = SessionToolState::for_test();
        let profile = outside.join("profile");
        std::fs::create_dir_all(profile.join(".ssh").join("keys")).expect("create");
        let known_hosts = profile.join(".ssh").join("known_hosts");
        std::fs::write(&known_hosts, "KNOWN-HOSTS-ENTRY").expect("write");
        std::fs::write(profile.join(".ssh").join("id_rsa"), "PRIVATE-KEY").expect("write");
        let previous = std::env::var_os("USERPROFILE");
        unsafe { std::env::set_var("USERPROFILE", &profile) };

        let ssh = profile.join(".ssh");
        let ssh_path = ssh
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let listing = format!("[System.IO.Directory]::GetFileSystemEntries('{ssh_path}')");
        let read_key = format!(
            "[System.IO.File]::ReadAllText('{}')",
            profile
                .join(".ssh")
                .join("id_rsa")
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/")
        );

        let granted = |command: String| {
            let state = state.clone();
            let workspace = workspace.clone();
            let grant = ssh.clone();
            async move {
                tool::execute(
                    &call(
                        "ExecCommand",
                        serde_json::json!({
                            "command": command,
                            "sandbox_permissions": "with_additional_permissions",
                            "additional_permissions": {
                                "file_system": { "read": [grant.to_string_lossy()] }
                            }
                        }),
                    ),
                    ToolContext {
                        cwd: &workspace,
                        tracker: &mut ChangeTracker::default(),
                        state: &state,
                        fs_grant: None,
                        sandbox_override: Default::default(),
                        sandbox_profile: pig_core::task::SandboxProfile::WorkspaceWrite,
                        additional_permissions: pig_core::task::AdditionalPermissions::read_roots(
                            vec![grant.clone()],
                        ),
                    },
                )
                .await
                .0
            }
        };
        let listed = granted(listing).await;
        let key = granted(read_key).await;
        if let Some(previous) = previous {
            unsafe { std::env::set_var("USERPROFILE", previous) };
        } else {
            unsafe { std::env::remove_var("USERPROFILE") };
        }

        assert_succeeded(&listed);
        assert!(listed.contains("known_hosts"), "{listed}");
        assert!(listed.contains("id_rsa"), "{listed}");
        assert_failed(&key);
        assert!(!key.contains("PRIVATE-KEY"), "{key}");
    }

    /// Deleting a `[[sandbox.deny]]` group really stops it denying: the config
    /// table reaches the kernel policy. `.env` is used because it is the group
    /// users most plausibly need to remove (repositories of example env files).
    ///
    /// The policy is rebuilt per launch, so a configuration change applies to
    /// the next command. The settings are reset **before** any assertion that
    /// could panic, so a failure here cannot leave the process-wide slot
    /// pointing at a weakened table and pollute the following tests.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_a_deny_group_can_be_deleted() {
        let _guard = sandbox_lock().await;
        let (workspace, _) = fixture("deleted-group");
        let state = SessionToolState::for_test();
        std::fs::write(workspace.join(".env"), "DELETED-GROUP-VISIBLE").expect("write env fixture");

        // The seeded table denies it.
        install_seeded_settings();
        let denied = run(&workspace, &state, &cat(".env")).await;
        let denied_ok = !denied.contains("DELETED-GROUP-VISIBLE");

        // The same table without its `.env` group, installed through the same
        // entry point a config load uses.
        let mut config = pig_protocol::AppConfig::default();
        config.sandbox.deny = Some(pig_core::config::default_deny_groups()[1..].to_vec());
        pig_core::task::init_sandbox_settings(pig_core::task::SandboxSettings::from_config(
            &config,
        ));
        let allowed = run(&workspace, &state, &cat(".env")).await;

        // Other groups are untouched by that deletion.
        std::fs::write(workspace.join("id_rsa"), "SSH-STILL-SECRET").expect("write key");
        let ssh = run(&workspace, &state, &cat("id_rsa")).await;
        let ssh_ok = !ssh.contains("SSH-STILL-SECRET");

        // Reset the process-wide settings before asserting anything.
        reset_sandbox_settings();

        assert!(denied_ok, "the seeded group must deny by default: {denied}");
        assert!(ssh_ok, "deleting env must not delete ssh: {ssh}");
        assert_succeeded(&allowed);
        assert!(allowed.contains("DELETED-GROUP-VISIBLE"), "{allowed}");
    }

    /// Workspace metadata is writable by the *user* but not by the sandboxed
    /// model: those files drive processes running outside the sandbox (git
    /// hooks, pig-code's own project hooks/MCP servers, permission rules), so a
    /// model write there is code execution on the next launch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_protects_workspace_metadata() {
        let _guard = sandbox_lock().await;
        let (workspace, _) = fixture("metadata");
        let state = SessionToolState::for_test();

        // A project directory the app itself creates, plus a repo-shaped `.git`.
        std::fs::create_dir_all(workspace.join(".pigcode").join("plans")).expect("create .pigcode");
        std::fs::write(workspace.join(".pigcode").join("hooks.toml"), "[hooks]\n")
            .expect("write hooks fixture");
        std::fs::create_dir_all(workspace.join(".git").join("hooks")).expect("create .git/hooks");
        std::fs::write(
            workspace.join(".git").join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .expect("write git HEAD fixture");
        std::fs::write(
            workspace.join(".git").join("hooks").join("pre-commit"),
            "#!/bin/sh\n",
        )
        .expect("write git hook fixture");

        // Writing the project hook configuration (core executes it outside the
        // sandbox) must be denied.
        let hooks = run(
            &workspace,
            &state,
            &write_file(".pigcode/hooks.toml", "malicious"),
        )
        .await;
        assert_failed(&hooks);
        assert_eq!(
            std::fs::read_to_string(workspace.join(".pigcode").join("hooks.toml"))
                .expect("host read"),
            "[hooks]\n",
            "the file must be unchanged: {hooks}"
        );

        // Planting a git hook must be denied too.
        let hook = run(
            &workspace,
            &state,
            &write_file(".git/hooks/pre-commit", "malicious"),
        )
        .await;
        assert_failed(&hook);

        // `.git` is not frozen as a whole: `git add`/`commit` write the index
        // and objects, and the model has to be able to commit.
        std::fs::create_dir_all(workspace.join(".git").join("objects"))
            .expect("create .git/objects");
        let index = run(
            &workspace,
            &state,
            &write_file(".git/objects/probe", "data"),
        )
        .await;
        assert_succeeded(&index);

        // Reading metadata stays allowed: `git status` has to work.
        let head = run(&workspace, &state, &cat(".git/HEAD")).await;
        assert_succeeded(&head);
        assert!(head.contains("refs/heads/main"), "{head}");

        // Ordinary workspace writes are unaffected.
        std::fs::write(workspace.join("src.txt"), "ok").expect("write plain fixture");
        let plain = run(&workspace, &state, &append_file("src.txt", "-appended")).await;
        assert_succeeded(&plain);
        assert_eq!(
            std::fs::read_to_string(workspace.join("src.txt")).expect("host read"),
            "ok-appended"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_background_commands_use_the_same_boundary() {
        let _guard = sandbox_lock().await;
        let (workspace, outside) = fixture("background");
        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();
        let (out, _) = bash(
            &workspace,
            &mut tracker,
            &state,
            serde_json::json!({
                "command": write_file("../outside/background.txt", "denied"),
                "run_in_background": true
            }),
        )
        .await;
        let task_id = out
            .strip_prefix("Started in the background, task_id: ")
            .and_then(|rest| rest.split('.').next())
            .expect("background task id")
            .to_string();

        let mut finished = false;
        for _ in 0..100 {
            finished = state
                .tasks
                .lock()
                .expect("task registry")
                .iter()
                .any(|entry| {
                    entry.id == task_id
                        && matches!(
                            &entry.status,
                            pig_protocol::TaskStatus::Completed
                                | pig_protocol::TaskStatus::Failed
                                | pig_protocol::TaskStatus::Killed
                        )
                });
            if finished {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert!(finished, "background command did not finish");
        assert!(!outside.join("background.txt").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sandbox_preserves_the_exit_code() {
        let _guard = sandbox_lock().await;
        let (workspace, _) = fixture("exit-code");
        let state = SessionToolState::for_test();
        let command = if shell_kind() == "cmd" {
            "exit /b 3"
        } else {
            "exit 3"
        };

        let output = run(&workspace, &state, command).await;

        assert!(output.contains("[exit code: 3]"), "{output}");
    }
}
