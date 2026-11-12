use super::*;
use pig_protocol::TaskStatus;
use std::path::{Path, PathBuf};

/// Queued is structured state; acquiring a slot only changes that state.
#[test]
fn queued_registration_marks_pending() {
    let state = SessionToolState::for_test();
    let task_id = register_agent_task_queued(
        &state,
        "subagent explore: investigate the issue".to_string(),
        tokio_util::sync::CancellationToken::new(),
        "a1-1".to_string(),
    );
    {
        let tasks = state.tasks.lock().expect("task registry lock");
        let entry = tasks
            .iter()
            .find(|t| t.id == task_id)
            .expect("entry exists");
        assert_eq!(entry.status, TaskStatus::Queued);
        assert_eq!(entry.command, "subagent explore: investigate the issue");
    }
    mark_agent_task_started(&state, &task_id);
    {
        let tasks = state.tasks.lock().expect("task registry lock");
        let entry = tasks
            .iter()
            .find(|t| t.id == task_id)
            .expect("entry exists");
        assert_eq!(entry.status, TaskStatus::Running);
        assert_eq!(entry.command, "subagent explore: investigate the issue");
    }
    mark_agent_task_started(&state, "b999");
}

/// The panel gets the exact spill path from the registry snapshot (it must not
/// reconstruct it from the workspace) and shell tasks point under the session
/// data directory.
#[test]
fn task_snapshot_carries_the_session_data_dir_spill_path() {
    let state = SessionToolState::for_test();
    let path = state.spill_path("b1").expect("data dir attached");
    {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        tasks.push(TaskEntry {
            id: "b1".to_string(),
            kind: pig_protocol::TaskKind::Shell,
            command: "make".to_string(),
            status: TaskStatus::Running,
            exit_code: None,
            reason: None,
            started_at: 1,
            ended_at: None,
            pid: None,
            output: "tail".to_string(),
            spill: Some(super::spill::Spill::new(path.clone())),
            cancel: None,
            agent_id: None,
            owner_agent_id: None,
            record_path: None,
            result_path: None,
            turns: None,
            model: None,
            terminal_notification_suppressed: false,
            foreground: false,
        });
    }
    let tasks = snapshot(&state.tasks);
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].spill_path.as_deref(),
        Some(path.as_path()),
        "the snapshot carries the spill path verbatim"
    );
    assert!(
        path.starts_with(state.data_dir().expect("data dir")),
        "the spill path is data-dir scoped"
    );
}

/// A missing executable produces a failed task and notification instead of
/// panicking or leaving a permanently running entry.
#[tokio::test]
async fn spawn_failure_produces_a_structured_failed_terminal() {
    let state = SessionToolState::for_test();
    let root = std::env::temp_dir().join(format!("pig-missing-shell-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("workspace");
    let mut shell = super::shell::default_model_shell();
    shell.program = root.join("missing-shell");
    let task_id = super::process::spawn_background_with_profile(
        &state,
        &root,
        &root,
        "echo should-not-run",
        &shell,
        super::sandbox::SandboxOverride::NoOverride,
        super::sandbox::SandboxProfile::Unrestricted,
        &super::sandbox::AdditionalPermissions::default(),
    );
    let tasks = state.tasks.lock().expect("tasks");
    let entry = tasks
        .iter()
        .find(|entry| entry.id == task_id)
        .expect("task");
    assert_eq!(entry.status, TaskStatus::Failed);
    assert_eq!(entry.exit_code, Some(-1));
    assert!(entry.ended_at.is_some());
    assert!(entry.reason.is_some());
    let notification = super::notification::terminal_notification(&state.session_id, entry);
    assert_eq!(notification.status, TaskStatus::Failed);
    assert!(matches!(
        notification.output,
        Some(pig_protocol::TaskNotificationOutput::Preview { ref text, .. })
            if text.contains("Failed to start")
    ));
}

/// An active network block cannot be re-created outside the sandbox, so an
/// approved `require_escalated` must keep the sandbox on even when no
/// explicit read-deny rule exists (docs/WINDOWS_SANDBOX.md). Without this
/// the approval silently ran unsandboxed and dropped the network policy.
#[test]
fn a_network_block_keeps_the_sandbox_for_require_escalated() {
    let state = SessionToolState::for_test();
    let cwd = std::env::temp_dir();
    let empty = || {
        sandbox::DenialView::for_settings(&sandbox::SandboxSettings::new(Vec::new()), &state, &cwd)
    };
    let blocked = SandboxDenialView {
        inner: empty(),
        network_blocked: true,
    };
    assert!(blocked.requires_sandbox_for_denials());
    let open = SandboxDenialView {
        inner: empty(),
        network_blocked: false,
    };
    assert!(
        !open.requires_sandbox_for_denials(),
        "no restriction and no network block leaves the unsandboxed form available"
    );
}

/// One live writer per subagent context: a second claim is refused, and
/// the claim is released by drop, including when the holder unwinds.
#[test]
fn agent_writer_claim_is_exclusive_and_panic_safe() {
    let state = SessionToolState::for_test();
    assert!(!state.agent_writer_active("a1-1"));
    let guard = state.try_claim_agent_writer("a1-1").expect("first claim");
    assert!(state.agent_writer_active("a1-1"));
    assert!(
        state.try_claim_agent_writer("a1-1").is_none(),
        "a second writer must not open the same context"
    );
    drop(guard);
    assert!(!state.agent_writer_active("a1-1"));

    let panic_state = state.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = panic_state
            .try_claim_agent_writer("a2-2")
            .expect("claim before panic");
        panic!("driver panic");
    }));
    assert!(result.is_err());
    assert!(
        !state.agent_writer_active("a2-2"),
        "unwinding drops the writer claim"
    );
    assert!(
        state.try_claim_agent_writer("a2-2").is_some(),
        "resume is possible once the driver has terminated"
    );
}

/// A registered Running task blocks resume even before a writer claim is
/// observable; the helper returns its task_id for the model-facing note.
#[test]
fn running_agent_task_is_reported() {
    let state = SessionToolState::for_test();
    let task_id = register_agent_task(
        &state,
        "subagent explore: test".to_string(),
        tokio_util::sync::CancellationToken::new(),
        "a3-3".to_string(),
    );
    assert_eq!(state.running_agent_task("a3-3"), Some(task_id.clone()));
    {
        let mut tasks = state.tasks.lock().expect("task registry lock");
        tasks
            .iter_mut()
            .find(|task| task.id == task_id)
            .expect("entry")
            .status = TaskStatus::Killed;
    }
    assert!(state.running_agent_task("a3-3").is_none());
}

/// Concurrency slots: a free slot is acquired immediately and drop returns
/// it; when full, an already-cancelled token gives up the queue without
/// taking a slot. (The static semaphore is process-wide; this is the only
/// test in the whole suite that touches it, so there is no concurrent
/// interference)
#[tokio::test]
async fn subagent_slot_acquire_and_cancel() {
    let _test_lock = SUBAGENT_SLOT_TEST_LOCK.lock().await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let permit = acquire_subagent_slot(&cancel)
        .await
        .expect("a free slot should be available immediately");
    assert_eq!(subagent_slots_available(), MAX_CONCURRENT_SUBAGENTS - 1);
    drop(permit);
    assert_eq!(subagent_slots_available(), MAX_CONCURRENT_SUBAGENTS);
    // Fill up: acquire with an already-cancelled token immediately returns None
    let mut held = Vec::new();
    for _ in 0..MAX_CONCURRENT_SUBAGENTS {
        held.push(
            acquire_subagent_slot(&cancel)
                .await
                .expect("all slots acquirable before filling up"),
        );
    }
    assert_eq!(subagent_slots_available(), 0);
    let cancelled = tokio_util::sync::CancellationToken::new();
    cancelled.cancel();
    assert!(acquire_subagent_slot(&cancelled).await.is_none());
    drop(held);
    assert_eq!(subagent_slots_available(), MAX_CONCURRENT_SUBAGENTS);
}

/// `git --exec-path` output → install root: MINGW segment lookup (git
/// outputs forward-slash paths). Windows version: asserts the native
/// backslash form (Path equality compares components, so `C:\...` matches
/// the components of the forward-slash path the function returns).
#[cfg(windows)]
#[test]
fn exec_path_root_inference() {
    let root = root_from_exec_path_text("C:/Program Files/Git/mingw64/libexec/git-core\n")
        .expect("the standard layout should hit");
    assert_eq!(root, PathBuf::from("C:\\Program Files\\Git"));

    let root = root_from_exec_path_text("C:/Git/ucrt64/libexec/git-core").unwrap();
    assert_eq!(root, PathBuf::from("C:\\Git"));

    // Shim layout: an mingw segment at the very front still yields the drive root
    let root = root_from_exec_path_text("D:/mingw64/libexec/git-core").unwrap();
    assert_eq!(root, PathBuf::from("D:\\"));

    // No MINGW segment: fall back two levels up (libexec/git-core → root)
    let root = root_from_exec_path_text("C:/x/libexec/git-core").unwrap();
    assert_eq!(root, PathBuf::from("C:\\x"));

    assert!(root_from_exec_path_text("").is_none());
}

/// Unix version: verifies the same logic with native Unix paths (in
/// production this function is only called on the Windows detection chain;
/// kept here for logic regression coverage).
#[cfg(not(windows))]
#[test]
fn exec_path_root_inference() {
    let root = root_from_exec_path_text("/opt/Git/mingw64/libexec/git-core\n")
        .expect("the standard layout should hit");
    assert_eq!(root, PathBuf::from("/opt/Git"));

    let root = root_from_exec_path_text("/opt/Git/ucrt64/libexec/git-core").unwrap();
    assert_eq!(root, PathBuf::from("/opt/Git"));

    // Shim layout: an mingw segment right after the root still yields the root
    let root = root_from_exec_path_text("/mingw64/libexec/git-core").unwrap();
    assert_eq!(root, PathBuf::from("/"));

    // No MINGW segment: fall back two levels up (libexec/git-core → root)
    let root = root_from_exec_path_text("/opt/x/libexec/git-core").unwrap();
    assert_eq!(root, PathBuf::from("/opt/x"));

    assert!(root_from_exec_path_text("").is_none());
}

/// Infer bash candidates from the git.exe layout: take the grandparent under cmd/bin; other layouts yield no candidates.
#[test]
fn git_bash_candidate_inference() {
    let candidates = git_bash_candidates(Path::new("C:/Git/cmd/git.exe"));
    assert_eq!(
        candidates,
        vec![
            PathBuf::from("C:/Git/bin/bash.exe"),
            PathBuf::from("C:/Git/usr/bash.exe")
        ]
    );
    // A shim directory (not cmd/bin) should yield no candidates
    assert!(git_bash_candidates(Path::new("C:/shim/git.exe")).is_empty());
}

/// Git Bash detection must never trust a bare bash.exe on the PATH:
/// `C:\Windows\System32\bash.exe` is the WSL launcher (seen in the wild
/// 2026-10-08 — every Bash command ran inside WSL, dotnet unreachable,
/// `-c` payload re-expanded by an outer WSL shell). The git.exe layout
/// inference must win instead. Cross-platform: dummy files in an isolated
/// temp layout, the real process env is untouched.
#[test]
fn git_bash_detection_ignores_bare_bash_exe_on_path() {
    let tmp = std::env::temp_dir().join(format!("pig-git-bash-detect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    // System32-style dir first on PATH with a bare bash.exe (the WSL trap)
    let sys32 = tmp.join("Sys32");
    // scoop-style git layout on a later PATH entry: cmd/git.exe + bin/bash.exe
    let git_root = tmp.join("gitroot");
    for dir in [&sys32, &git_root.join("cmd"), &git_root.join("bin")] {
        std::fs::create_dir_all(dir).expect("create layout dir");
    }
    std::fs::write(sys32.join("bash.exe"), b"").expect("write fake WSL launcher");
    std::fs::write(git_root.join("cmd").join("git.exe"), b"").expect("write fake git");
    std::fs::write(git_root.join("bin").join("bash.exe"), b"").expect("write fake bash");

    let found = git_bash_from_dirs(&[sys32.clone(), git_root.join("cmd")], &[], &|p: &Path| {
        p.is_file()
    });
    assert_eq!(
        found,
        Some(git_root.join("bin").join("bash.exe")),
        "a bare bash.exe ahead of a real git layout must be ignored"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// NUL redirect rewrite: only the redirect target is touched, not plain arguments.
#[test]
fn nul_redirect_rewrite() {
    assert_eq!(
        rewrite_nul_redirects("ipconfig > NUL 2>&1 && echo ok"),
        "ipconfig > /dev/null 2>&1 && echo ok"
    );
    assert_eq!(rewrite_nul_redirects("dir >>nul"), "dir >>/dev/null");
    assert_eq!(
        rewrite_nul_redirects("echo NUL is a word"),
        "echo NUL is a word"
    );
}

/// Unix shell detection: pick the first existing bash candidate path; fall back to sh when none exist.
#[cfg(not(windows))]
#[test]
fn unix_shell_prefers_bash_falls_back_to_sh() {
    let expected = ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
        .unwrap_or("sh");
    assert_eq!(unix_shell(), expected);
}

/// When git is installed (a hard Pig Code dependency), detection must find
/// Git Bash. Windows-only: the detection chain looks for .exe/ProgramFiles
/// layouts, while on Unix spawn goes straight to sh without this chain
/// (running it here would necessarily fail, which is not a regression
/// signal).
#[cfg(windows)]
#[test]
fn detection_finds_bash_when_git_present() {
    let git_on_path = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !git_on_path {
        return; // Skip on machines without git (theoretically nonexistent: Pig Code hard-depends on git)
    }
    assert!(
        matches!(windows_shell(), WindowsShell::GitBash(_)),
        "git is available but Git Bash was not detected"
    );
}
