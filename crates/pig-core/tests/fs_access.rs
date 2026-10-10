//! Outside-workspace read/write switches (fs_read_outside / fs_write_outside)
//! plus tmp directory allowance and outside symlink/sensitive-file semantics
//! and plan-mode orthogonality.

mod common;

use std::sync::atomic::Ordering;

use pig_core::task::SessionToolState;
use pig_core::tool::{self, ChangeTracker, ToolContext};
use pig_protocol::{Event, ExecMode, Op};
use pig_provider::ToolCall;

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
            fs_grant: None,
        },
    )
    .await;
    (out, is_error)
}

/// Outside file: in the workspace's parent directory (a relative-escape target, not an absolute tmp request)
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
    assert!(out.contains("Path escapes the working directory"), "{out}");
    assert!(
        out.contains("outside-workspace read access"),
        "read guidance copy: {out}"
    );

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": rel, "content": "x\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("Path escapes the working directory"), "{out}");
    assert!(
        out.contains("outside-workspace write access"),
        "write guidance copy: {out}"
    );

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

    // Outside write (create) succeeds
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

    // Outside read succeeds
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
    // An explicit absolute path into the system tmp (outside the workspace): allowed even with all switches off
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
    assert!(!is_error, "tmp write should be allowed: {out}");

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": target_str}),
    )
    .await;
    assert!(!is_error, "tmp read should be allowed: {out}");
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

    // Off: denied
    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "alias.txt"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("Path escapes the working directory"), "{out}");

    // On: the same gate allows it (including symlinks pointing outside)
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
    assert!(
        out.contains("sensitive file"),
        "sensitive files must stay blocked even with the switch on: {out}"
    );
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
    assert!(out.contains("sensitive file"), "{out}");

    let _ = std::fs::remove_dir_all(&outside);
}

/// Plan mode's hard deny is orthogonal to the outside-write switch: with all
/// switches on, Write under Plan is still hard-denied wholesale.
/// Also exercises the Op::SetFsAccess path (handled in agent_loop).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_blocks_write_even_with_switch_on() {
    let (config_path, cwd, data_dir) = common::setup("fs-plan");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, cwd.clone()).await;

    // All switches on + plan mode (orthogonal to execution mode: tier stays AutoEdit, the plan hard deny still applies)
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
        .send(Op::SetPlanMode {
            session_id: session_id.clone(),
            enabled: true,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{} edit a file", pig_provider::mock::SCENARIO_B_TRIGGER),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
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
        "no approval should pop up in Plan mode"
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
    assert!(!ends.is_empty(), "tool calls did happen (hard-denied)");
    assert!(
        ends.iter().all(|out| out.contains("Plan mode")),
        "all hard-denied by Plan mode: {ends:?}"
    );
    assert!(
        !cwd.join(pig_provider::mock::SCENARIO_B_FILE).exists(),
        "no file should be created in Plan mode"
    );
    agent.shutdown();
}

/// Session state with extra read-only roots (for the data_dir/sessions allowlist test)
fn state_with_extra_roots(roots: Vec<std::path::PathBuf>) -> SessionToolState {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (wake_tx, _wake_rx) = tokio::sync::mpsc::unbounded_channel();
    SessionToolState::new("test".to_string(), tx, wake_tx, roots)
}

/// A non-tmp fixture root: the tmp exemption allows every absolute path inside
/// tmp, so the allowlist boundary cannot be tested under tmp; CARGO_TARGET_TMPDIR
/// (target/tmp/) is cargo's scratch directory for integration tests.
fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("pig-fs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// extra_read_roots (the data_dir/sessions subtree) is always readable:
/// absolute-path reads inside a root are allowed, absolute paths outside the
/// root (config.toml at the data_dir root) are still denied, writes inside the
/// root are still denied (reads only), and relative ../ escapes into the root
/// are not allowed (same rule as the tmp exemption: applies only to
/// absolute-path requests).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extra_read_roots_allow_read_only() {
    let base = scratch_dir("extra-roots");
    let workspace = base.join("workspace");
    let sessions = base.join("data").join("sessions");
    let record_dir = sessions.join("s1.agents");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&record_dir).unwrap();
    std::fs::write(
        record_dir.join("a1.result.md"),
        "full subagent result text\n",
    )
    .unwrap();
    std::fs::write(base.join("data").join("config.toml"), "placeholder\n").unwrap();

    let mut tracker = ChangeTracker::default();
    let state = state_with_extra_roots(vec![sessions.clone()]);

    // 1. Absolute-path read inside the root: allowed
    let result_md = record_dir.join("a1.result.md");
    let (out, is_error) = run(
        &workspace,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": result_md.to_string_lossy()}),
    )
    .await;
    assert!(
        !is_error,
        "read inside the whitelist should be allowed: {out}"
    );
    assert!(out.contains("full subagent result text"), "{out}");

    // 2. Absolute path at the data_dir root, outside the root (config.toml): still denied
    // (that is the real layout — config.toml sits at the data_dir root, not under sessions/, so it is naturally excluded)
    let config = base.join("data").join("config.toml");
    let (out, is_error) = run(
        &workspace,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": config.to_string_lossy()}),
    )
    .await;
    assert!(
        is_error,
        "outside the whitelist should still be denied: {out}"
    );
    assert!(out.contains("Path escapes the working directory"), "{out}");

    // 3. Write inside the root (absolute path): still denied (the exemption allows reads only)
    let new_file = record_dir.join("evil.md");
    let (out, is_error) = run(
        &workspace,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": new_file.to_string_lossy(), "content": "x\n"}),
    )
    .await;
    assert!(
        is_error,
        "whitelist only allows reads, writes still denied: {out}"
    );
    assert!(!new_file.exists(), "write must not land on disk");

    // 4. Relative ../ escape into the root: not allowed (aligned with tmp's absolute-path rule)
    let (out, is_error) = run(
        &workspace,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "../data/sessions/s1.agents/a1.result.md"}),
    )
    .await;
    assert!(
        is_error,
        "relative escape must not get the whitelist exemption: {out}"
    );
    assert!(out.contains("Path escapes the working directory"), "{out}");

    let _ = std::fs::remove_dir_all(&base);
}

/// Root reached via a symlink: after construction-time canonicalization of the root, reads via the link path are still allowed.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extra_read_roots_root_via_symlink() {
    let base = scratch_dir("extra-roots-link");
    let workspace = base.join("workspace");
    let real_sessions = base.join("data").join("sessions");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&real_sessions).unwrap();
    std::fs::write(real_sessions.join("a1.result.md"), "linked result\n").unwrap();
    // Pass the link path as the root (canonicalized inside new()) and also issue the read via the link path
    let link = base.join("link-sessions");
    std::os::unix::fs::symlink(&real_sessions, &link).unwrap();

    let mut tracker = ChangeTracker::default();
    let state = state_with_extra_roots(vec![link.clone()]);
    let target = link.join("a1.result.md");
    let (out, is_error) = run(
        &workspace,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": target.to_string_lossy()}),
    )
    .await;
    assert!(
        !is_error,
        "read via a symlinked root should be allowed: {out}"
    );
    assert!(out.contains("linked result"), "{out}");

    let _ = std::fs::remove_dir_all(&base);
}
