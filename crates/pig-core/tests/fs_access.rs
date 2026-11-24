//! Outside-workspace policy after the Codex alignment: full-disk read, writes
//! limited to the workspace/tmp/extra roots with a per-call approval, plus
//! sensitive-file and plan-mode orthogonality.

mod common;

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

/// A workspace sandbox layer carrying explicit deny rules (project layers are
/// add-only; the tests use the same filesystem shape the config parser emits).
fn project_denies(rules: &[&str]) -> pig_core::config::ProjectSandboxLayer {
    pig_core::config::ProjectSandboxLayer {
        filesystem: Some(
            rules
                .iter()
                .map(|rule| {
                    (
                        rule.to_string(),
                        pig_protocol::SandboxFsPermission::Access(
                            pig_protocol::SandboxFsAccess::Deny,
                        ),
                    )
                })
                .collect(),
        ),
        network_block: None,
    }
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
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    (out, is_error)
}

/// Like `run`, but with the per-call outside-write grant the approval gate sets.
async fn run_granted(
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
            fs_grant: Some(tool::FsAccess::Write),
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    (out, is_error)
}

/// Like `run`, but with the one-call read roots an approved credential file
/// (or directory listing) carries.
async fn run_with_read_roots(
    dir: &std::path::Path,
    tracker: &mut ChangeTracker,
    state: &SessionToolState,
    name: &str,
    args: serde_json::Value,
    roots: Vec<std::path::PathBuf>,
) -> (String, bool) {
    let (out, is_error, _, _, _) = tool::execute(
        &call(name, args),
        ToolContext {
            cwd: dir,
            tracker,
            state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: pig_core::task::AdditionalPermissions::read_roots(roots),
        },
    )
    .await;
    (out, is_error)
}

/// Creates an isolated directory outside both the workspace and system tmp.
fn outside_dir(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let _ = dir;
    let path = common::outside_dir(name);
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Reads outside the workspace are allowed everywhere (full-disk read), with
/// no approval and no switch to turn on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_read_is_allowed() {
    let dir = temp_dir("read-allowed");
    let outside = outside_dir(&dir, "outside-read");
    std::fs::write(outside.join("note.txt"), "outside-content\n").unwrap();
    std::fs::write(outside.join("second.txt"), "second-content\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // Absolute paths outside the workspace. Distinct files: Read's repeat-read
    // cache keys on the resolved path, so re-reading the same one would only
    // report "unchanged".
    for (name, expected) in [
        ("note.txt", "outside-content"),
        ("second.txt", "second-content"),
    ] {
        let (out, is_error) = run(
            &dir,
            &mut tracker,
            &state,
            "Read",
            serde_json::json!({"path": outside.join(name).to_string_lossy()}),
        )
        .await;
        assert!(!is_error, "outside read must be allowed: {out}");
        assert!(out.contains(expected), "{out}");
    }

    let _ = std::fs::remove_dir_all(&outside);
}

/// Writes outside the workspace are denied without the per-call approval grant
/// and succeed with it (the gate sets `fs_grant` after the user allows).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_write_needs_the_per_call_grant() {
    let dir = temp_dir("write-gated");
    let outside = outside_dir(&dir, "outside-write");
    let target = outside.join("created.txt");
    let path = target.to_string_lossy().to_string();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": path, "content": "hello\n"}),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("Path escapes the working directory"), "{out}");
    assert!(
        out.contains("needs the user's approval"),
        "the error must explain the approval route: {out}"
    );
    assert!(!target.exists(), "the unapproved write must not land");

    let (out, is_error) = run_granted(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": path, "content": "hello\n"}),
    )
    .await;
    assert!(!is_error, "the granted write must succeed: {out}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello\n");

    let _ = std::fs::remove_dir_all(&outside);
}

/// An absolute path into the system tmp stays exempt in both directions: tmp is
/// scratch space, not part of the workspace boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tmp_absolute_path_allowed() {
    let dir = temp_dir("tmp-ok");
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

/// A symlink pointing outside the workspace: reads follow it (full-disk read),
/// writes do not (they need the per-call grant).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_symlink_read_follows_write_is_gated() {
    let dir = temp_dir("symlink-gate");
    let outside = temp_dir("symlink-gate-outside");
    std::fs::write(outside.join("data.txt"), "linked\n").unwrap();
    std::os::unix::fs::symlink(outside.join("data.txt"), dir.join("alias.txt")).unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "alias.txt"}),
    )
    .await;
    assert!(!is_error, "an outside symlink is readable: {out}");
    assert!(out.contains("linked"), "{out}");

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": "alias.txt", "content": "clobber\n"}),
    )
    .await;
    assert!(is_error, "an outside symlink must not be writable: {out}");
    assert_eq!(
        std::fs::read_to_string(outside.join("data.txt")).unwrap(),
        "linked\n"
    );
}

/// Windows counterpart of the outside-symlink test: a junction is a directory
/// reparse point, and the file tools must resolve it to the final object.
/// Reads still follow it (full-disk read), writes stay gated.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_junction_read_follows_write_is_gated() {
    let dir = temp_dir("junction-gate");
    let outside = outside_dir(&dir, "pig-junction-gate");
    std::fs::write(outside.join("data.txt"), "linked\n").unwrap();
    common::junction(&dir.join("alias"), &outside);

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "alias/data.txt"}),
    )
    .await;
    assert!(
        !is_error,
        "a read through an outside junction is allowed: {out}"
    );
    assert!(out.contains("linked"), "{out}");

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": "alias/data.txt", "content": "clobber\n"}),
    )
    .await;
    assert!(
        is_error,
        "a write through an outside junction must be gated: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("data.txt")).unwrap(),
        "linked\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

/// The sensitive-name veto names the object actually opened, so a junction
/// whose target holds `.env` cannot carry it past the check.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_checks_follow_a_junction_to_a_sensitive_target() {
    let dir = temp_dir("junction-sensitive");
    let outside = outside_dir(&dir, "pig-junction-sensitive");
    std::fs::write(outside.join(".env"), "ENV-SECRET").unwrap();
    std::fs::write(outside.join("notes.txt"), "benign\n").unwrap();
    common::junction(&dir.join("alias"), &outside);

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "alias/.env"}),
    )
    .await;
    assert!(
        is_error,
        "a junction must not carry .env past the veto: {out}"
    );
    assert!(!out.contains("ENV-SECRET"), "{out}");

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "alias/notes.txt"}),
    )
    .await;
    assert!(!is_error, "a benign junction target stays readable: {out}");
    assert!(out.contains("benign"), "{out}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

/// An alternate data stream lives on its base file: a sensitive base name is
/// refused through any stream suffix, a template base keeps its exception, and
/// a benign file's stream is a real readable object.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alternate_data_streams_keep_the_base_file_name_rules() {
    let dir = temp_dir("ads");
    std::fs::write(dir.join(".env"), "ENV-BASE").unwrap();
    std::fs::write(dir.join(".env.example"), "TEMPLATE").unwrap();
    std::fs::write(dir.join("notes.txt"), "BASE").unwrap();
    std::fs::write(dir.join("notes.txt:stream"), "STREAM-DATA").unwrap();
    std::fs::write(dir.join(".env:secret"), "ENV-STREAM").unwrap();
    std::fs::write(dir.join(".env.example:readable"), "TEMPLATE-STREAM").unwrap();

    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "notes.txt:stream"}),
    )
    .await;
    assert!(!is_error, "a benign stream is readable: {out}");
    assert!(out.contains("STREAM-DATA"), "{out}");

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": ".env:secret"}),
    )
    .await;
    assert!(is_error, "a stream on a sensitive file is refused: {out}");
    assert!(!out.contains("ENV-STREAM"), "{out}");
    assert!(!out.contains("ENV-BASE"), "{out}");

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": ".env.example:readable"}),
    )
    .await;
    assert!(!is_error, "the template exception survives a stream: {out}");
    assert!(out.contains("TEMPLATE-STREAM"), "{out}");

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": ".env:secret", "content": "clobber\n"}),
    )
    .await;
    assert!(is_error, "a write to a sensitive stream is refused: {out}");
    assert_eq!(
        std::fs::read_to_string(dir.join(".env")).unwrap(),
        "ENV-BASE"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A symlinked file is read through the link, while Edit/Write replace the
/// link entry itself: reading the spelling and then editing it keeps working,
/// and the target stays untouched.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_file_can_be_read_then_edited_by_its_spelling() {
    let dir = temp_dir("symlink-edit");
    std::fs::write(dir.join("target.txt"), "before\n").unwrap();
    std::os::unix::fs::symlink("target.txt", dir.join("alias.txt")).unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "alias.txt"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("before"), "{out}");

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Edit",
        serde_json::json!({"path": "alias.txt", "old_string": "before", "new_string": "after"}),
    )
    .await;
    assert!(
        !is_error,
        "a read symlink spelling must stay editable: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("alias.txt")).unwrap(),
        "after\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("target.txt")).unwrap(),
        "before\n",
        "the write replaces the link entry, not the target"
    );

    // Write follows the same rule.
    std::fs::remove_file(dir.join("alias.txt")).unwrap();
    std::os::unix::fs::symlink("target.txt", dir.join("alias.txt")).unwrap();
    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "alias.txt"}),
    )
    .await;
    assert!(!is_error, "{out}");
    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Write",
        serde_json::json!({"path": "alias.txt", "content": "written\n"}),
    )
    .await;
    assert!(
        !is_error,
        "a read symlink spelling must stay writable: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("alias.txt")).unwrap(),
        "written\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("target.txt")).unwrap(),
        "before\n",
        "the target is untouched"
    );
}

/// A link repointed after the read must not let Edit read the new target: the
/// read-through object gets the same deny vetoes as Read.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_refuses_a_repointed_symlink_target() {
    let dir = temp_dir("symlink-repoint");
    let denied = dir.join("denied.txt");
    let innocuous = dir.join("innocuous.txt");
    std::fs::write(&denied, "DENIED-SECRET\n").unwrap();
    std::fs::write(&innocuous, "innocuous-ok!\n").unwrap();
    let alias = dir.join("alias.txt");
    std::os::unix::fs::symlink("innocuous.txt", &alias).unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    *state.sandbox_project.write().expect("project layer") = project_denies(&["denied.txt"]);

    let (out, is_error) = run(
        &dir,
        &mut tracker,
        &state,
        "Read",
        serde_json::json!({"path": "alias.txt"}),
    )
    .await;
    assert!(!is_error, "{out}");

    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink("denied.txt", &alias).unwrap();
    let (out, is_error, _, edit, _) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "alias.txt", "old_string": "DENIED", "new_string": "LEAKED"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    assert!(
        is_error,
        "the denied read-through target must be refused: {out}"
    );
    assert!(out.contains("denied"), "{out}");
    assert!(edit.is_none(), "no change diff may carry denied content");
    assert_eq!(std::fs::read_to_string(&denied).unwrap(), "DENIED-SECRET\n");
}

/// An outside-write grant names the visible entry, not a denied target hidden
/// behind a link: the read-through veto cannot be lifted by that approval.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_grant_does_not_lift_a_hidden_deny_target() {
    let dir = temp_dir("symlink-grant-deny");
    std::fs::write(dir.join("denied.txt"), "DENIED-SECRET\n").unwrap();
    std::os::unix::fs::symlink("denied.txt", dir.join("alias.txt")).unwrap();
    let state = SessionToolState::for_test();
    let mut tracker = ChangeTracker::default();
    *state.sandbox_project.write().expect("project layer") = project_denies(&["denied.txt"]);

    let (out, is_error, _, edit, _) = tool::execute(
        &call(
            "Write",
            serde_json::json!({"path": "alias.txt", "content": "replacement\n"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: Some(tool::FsAccess::Write),
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    assert!(
        is_error,
        "the hidden target is denied regardless of the grant: {out}"
    );
    assert!(edit.is_none(), "no change diff may carry denied content");
    assert_eq!(
        std::fs::read_to_string(dir.join("denied.txt")).unwrap(),
        "DENIED-SECRET\n"
    );
    assert!(
        std::fs::symlink_metadata(dir.join("alias.txt"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the refused write leaves the link untouched"
    );
}

/// Sensitive files stay hard-blocked in both directions, and a write grant does
/// not unlock them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sensitive_files_are_always_blocked() {
    let dir = temp_dir("sensitive-outside");
    let outside = outside_dir(&dir, "sens-dir");
    std::fs::write(outside.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(dir.join(".env"), "INSIDE_SECRET=1\n").unwrap();
    let outside_env = outside.join(".env").to_string_lossy().to_string();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    for path in [outside_env, ".env".to_string()] {
        let (out, is_error) = run(
            &dir,
            &mut tracker,
            &state,
            "Read",
            serde_json::json!({"path": path}),
        )
        .await;
        assert!(is_error, "{out}");
        // Two layers refuse it: the sandbox veto (the built-in `env` group) and
        // the tool-level name check. Either message is a refusal, and neither
        // may leak the content.
        assert!(
            out.contains("sensitive file") || out.contains("sandbox deny rule"),
            "{out}"
        );
        assert!(!out.contains("SECRET=1"), "{out}");

        let (out, is_error) = run_granted(
            &dir,
            &mut tracker,
            &state,
            "Write",
            serde_json::json!({"path": path, "content": "x\n"}),
        )
        .await;
        assert!(is_error, "a grant must not unlock a sensitive file: {out}");
        assert!(
            out.contains("sensitive file") || out.contains("sandbox deny rule"),
            "{out}"
        );
    }

    let _ = std::fs::remove_dir_all(&outside);
}

/// Plan mode also runs Bash under the read-only sandbox profile. The tool gate
/// rejects Bash before it executes, so this pins the *first* line of defence;
/// the kernel profile is covered by `bash::windows_sandbox_read_only_profile_*`
/// and mirrors what an MCP/hook-spawned command would hit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_bash_cannot_write() {
    let (config_path, cwd, data_dir) = common::setup("fs-plan-bash");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, cwd.clone()).await;

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
            content: format!(
                "{} write via shell",
                pig_provider::mock::SCENARIO_PLAN_BASH_TRIGGER
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Full.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let events = common::recv_until(&agent.events, std::time::Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd {
                item_id,
                is_error: true,
                output,
                ..
            } if item_id.contains("call_pb_1") && output.contains("Plan mode")
        )),
        "the shell call must be rejected by plan mode: {events:?}"
    );
    assert!(
        !cwd.join(pig_provider::mock::PLAN_BASH_FILE).exists(),
        "plan mode must not create the file even in Full mode"
    );
    agent.shutdown();
}

/// Plan mode's hard deny is orthogonal to the outside-write rules: every write
/// is rejected wholesale, without any approval popup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_blocks_writes() {
    let (config_path, cwd, data_dir) = common::setup("fs-plan");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, cwd.clone()).await;

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
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
    // Bash is not a read-only tool, so plan mode rejects it too — otherwise a
    // shell redirection would be an unguarded write that bypasses the
    // plan-file-only rule.
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd {
                item_id,
                is_error: true,
                output,
                ..
            } if item_id.contains("call_b_bash") && output.contains("Plan mode")
        )),
        "Bash must be hard-denied in Plan mode: {events:?}"
    );
    assert!(
        !cwd.join(pig_provider::mock::SCENARIO_B_FILE).exists(),
        "no file should be created in Plan mode"
    );
    agent.shutdown();
}

/// Session state with extra read-only roots (for the data_dir/sessions allowlist test)
fn state_with_extra_roots(roots: Vec<std::path::PathBuf>) -> SessionToolState {
    let (tx, _rx) = async_channel::bounded(1024);
    SessionToolState::new("test".to_string(), tx, roots, vec![])
}

/// A non-tmp fixture root: the tmp exemption allows every absolute path inside
/// tmp, so the extra-root boundary cannot be tested under tmp;
/// CARGO_TARGET_TMPDIR (target/tmp/) is cargo's scratch directory for
/// integration tests.
fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("pig-fs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// extra_read_roots remain readable (now redundant with full-disk read, but the
/// exemption must not regress) and stay unwritable: they are a read allowlist,
/// not a write grant.
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

    let mut tracker = ChangeTracker::default();
    let state = state_with_extra_roots(vec![sessions.clone()]);

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
        "the whitelist only allows reads, writes still denied: {out}"
    );
    assert!(!new_file.exists(), "write must not land on disk");

    let _ = std::fs::remove_dir_all(&base);
}

/// Root reached via a symlink: after construction-time canonicalization of the
/// root, reads via the link path are still allowed.
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

/// The three alignments between the file tools and the shell sandbox.
///
/// The tools do not run under the sandbox, so without these checks a deny the
/// model shell enforces could be bypassed by asking Read instead of `cat` — and
/// a workspace-metadata write could install a hook.
mod tool_sandbox_alignment {
    use super::*;

    /// A workspace-metadata write is refused by Write and Edit even though the
    /// path is inside the workspace (the sandbox's carve-out list, shared).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metadata_writes_are_refused() {
        let dir = temp_dir("metadata-writes");
        std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
        std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
        std::fs::write(dir.join(".pigcode/hooks.toml"), "[[hooks]]\n").unwrap();
        std::fs::write(dir.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();

        for (path, content) in [
            (".pigcode/hooks.toml", "[[hooks]]\n"),
            (".git/hooks/pre-commit", "#!/bin/sh\ncurl evil\n"),
        ] {
            let (out, is_error) = run(
                &dir,
                &mut tracker,
                &state,
                "Write",
                serde_json::json!({"path": path, "content": content}),
            )
            .await;
            assert!(is_error, "{path} must be refused: {out}");
            assert!(out.contains("workspace metadata"), "{out}");
        }
        // The plan directory is the deliberate exception.
        let (out, is_error) = run(
            &dir,
            &mut tracker,
            &state,
            "Write",
            serde_json::json!({"path": ".pigcode/plans/plan-1.md", "content": "# plan\n"}),
        )
        .await;
        assert!(!is_error, "the plan file stays writable: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A credential directory is refused by Read even when the caller passes the
    /// **canonicalized** absolute path.
    ///
    /// This is the shape that broke before: `Read` resolves through
    /// `std::fs::canonicalize`, which on Windows returns `\?\C:\...`, while
    /// the credential roots are `dunce`-canonicalized without the prefix. The raw
    /// prefix comparison therefore never matched and `~/.ssh/known_hosts` read
    /// without any approval.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_credential_directory_is_refused_through_its_canonical_path() {
        let _env = credential_env_lock().await;
        let dir = temp_dir("credential-read");
        let profile = dir.join("profile");
        std::fs::create_dir_all(profile.join(".ssh")).unwrap();
        let known_hosts = profile.join(".ssh").join("known_hosts");
        std::fs::write(&known_hosts, "KNOWN-HOSTS-ENTRY").unwrap();
        let previous = std::env::var_os("USERPROFILE");
        unsafe { std::env::set_var("USERPROFILE", &profile) };

        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();
        let canonical = known_hosts.canonicalize().expect("canonical known_hosts");
        let read = run(
            &dir,
            &mut tracker,
            &state,
            "Read",
            serde_json::json!({"path": canonical.to_string_lossy()}),
        )
        .await;
        let grep = run(
            &dir,
            &mut tracker,
            &state,
            "Grep",
            serde_json::json!({
                "pattern": "KNOWN-HOSTS-ENTRY",
                "path": profile.join(".ssh").to_string_lossy()
            }),
        )
        .await;
        if let Some(previous) = previous {
            unsafe { std::env::set_var("USERPROFILE", previous) };
        } else {
            unsafe { std::env::remove_var("USERPROFILE") };
        }

        assert!(read.1, "a credential file must not read: {}", read.0);
        assert!(read.0.contains("credential directory"), "{}", read.0);
        assert!(!read.0.contains("KNOWN-HOSTS-ENTRY"), "{}", read.0);
        assert!(
            !grep.0.contains("KNOWN-HOSTS-ENTRY"),
            "Grep must not enter it either: {}",
            grep.0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The write side of the same alignment: a deny rule refuses Write too, so
    /// an approved outside write cannot silently override it (the refusal names
    /// the rule).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sandbox_denied_paths_are_not_writable() {
        let dir = temp_dir("deny-write");
        std::fs::write(dir.join("secret.pem"), "PEM-SECRET").unwrap();
        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();
        // Same reasoning as the read-side test: the workspace's own layer, not
        // the process-wide slot.
        *state.sandbox_project.write().expect("project layer") = project_denies(&["**/*.pem"]);

        let write = run(
            &dir,
            &mut tracker,
            &state,
            "Write",
            serde_json::json!({"path": "secret.pem", "content": "pwned\n"}),
        )
        .await;

        assert!(write.1, "a denied path must refuse the write: {}", write.0);
        assert!(write.0.contains("deny"), "{}", write.0);
        assert_eq!(
            std::fs::read_to_string(dir.join("secret.pem")).unwrap(),
            "PEM-SECRET"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Edit` cannot work on a credential file (it must read the current text to
    /// find `old_string`), so it says so; `Write` replaces the file with the
    /// user's per-file approval.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn credential_edits_are_refused_with_a_way_out() {
        let _env = credential_env_lock().await;
        let dir = temp_dir("credential-edit");
        let profile = dir.join("profile");
        std::fs::create_dir_all(profile.join(".ssh")).unwrap();
        let known_hosts = profile.join(".ssh").join("known_hosts");
        std::fs::write(&known_hosts, "old-entry").unwrap();
        let previous = std::env::var_os("USERPROFILE");
        unsafe { std::env::set_var("USERPROFILE", &profile) };

        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();
        let edit = run(
            &dir,
            &mut tracker,
            &state,
            "Edit",
            serde_json::json!({
                "path": known_hosts.to_string_lossy(),
                "old_string": "old-entry",
                "new_string": "new-entry"
            }),
        )
        .await;
        if let Some(previous) = previous {
            unsafe { std::env::set_var("USERPROFILE", previous) };
        } else {
            unsafe { std::env::remove_var("USERPROFILE") };
        }

        assert!(edit.1, "{}", edit.0);
        assert!(edit.0.contains("cannot be read"), "{}", edit.0);
        assert!(edit.0.contains("Use Write"), "{}", edit.0);
        assert_eq!(
            std::fs::read_to_string(&known_hosts).unwrap_or_default(),
            "old-entry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sensitive-name and deny-rule checks look at the object actually
    /// opened. A link named `alias.txt` must not carry `.env` or a `.pem`
    /// target past them.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_checks_follow_a_final_symlink_to_its_target() {
        let dir = temp_dir("symlink-sensitive-target");
        std::fs::write(dir.join(".env"), "ENV-SECRET").unwrap();
        std::fs::write(dir.join("secret.pem"), "PEM-SECRET").unwrap();
        std::os::unix::fs::symlink(".env", dir.join("env-alias.txt")).unwrap();
        std::os::unix::fs::symlink("secret.pem", dir.join("notes.txt")).unwrap();
        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();
        *state.sandbox_project.write().expect("project layer") = project_denies(&["**/*.pem"]);

        for path in ["env-alias.txt", "notes.txt"] {
            let (out, is_error) = run(
                &dir,
                &mut tracker,
                &state,
                "Read",
                serde_json::json!({"path": path}),
            )
            .await;
            assert!(is_error, "{path} must be refused: {out}");
            assert!(
                out.contains("sensitive file") || out.contains("denied") || out.contains("deny"),
                "{path}: {out}"
            );
            assert!(!out.contains("SECRET"), "{path}: {out}");

            let (out, is_error) = run(
                &dir,
                &mut tracker,
                &state,
                "ReadMediaFile",
                serde_json::json!({"path": path}),
            )
            .await;
            assert!(is_error, "{path} must be refused by ReadMediaFile: {out}");
            assert!(
                out.contains("sensitive file") || out.contains("denied") || out.contains("deny"),
                "{path}: {out}"
            );
        }

        let (out, _) = run(
            &dir,
            &mut tracker,
            &state,
            "Grep",
            serde_json::json!({"pattern": "SECRET", "path": "notes.txt"}),
        )
        .await;
        assert!(!out.contains("PEM-SECRET"), "{out}");
        assert!(!out.contains("ENV-SECRET"), "{out}");

        let (out, _) = run(
            &dir,
            &mut tracker,
            &state,
            "Glob",
            serde_json::json!({"pattern": "*", "path": "."}),
        )
        .await;
        assert!(
            !out.contains("env-alias.txt") && !out.contains("notes.txt"),
            "per-entry links are not listed as ordinary files: {out}"
        );
        assert!(!out.contains("SECRET"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session opened through a symlinked workspace spelling must still
    /// enforce workspace-anchored deny rules when `Grep`/`Glob` default to the
    /// workspace root: the rules compile against the canonical workspace, so
    /// every walked entry has to be resolved to that same object before the
    /// veto (otherwise a link spelling walks straight past the rule).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn default_search_root_resolves_a_symlinked_cwd_before_the_veto() {
        let real = temp_dir("symlinked-cwd-real");
        std::fs::create_dir_all(real.join("secrets")).unwrap();
        std::fs::write(real.join("secrets/hidden.txt"), "DENY-SECRET").unwrap();
        std::fs::write(real.join("notes.txt"), "PUBLIC-NOTE").unwrap();
        let link =
            std::env::temp_dir().join(format!("pig-core-fs-symlink-cwd-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();
        *state.sandbox_project.write().expect("project layer") = project_denies(&["secrets"]);

        // No `path` argument: the tool walks the session cwd, which is the
        // symlink spelling here.
        let (out, is_error) = run(
            &link,
            &mut tracker,
            &state,
            "Grep",
            serde_json::json!({"pattern": "PUBLIC-NOTE|DENY-SECRET"}),
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("PUBLIC-NOTE"), "{out}");
        assert!(
            !out.contains("DENY-SECRET"),
            "a workspace-anchored deny must hold through a symlinked cwd: {out}"
        );

        let (out, is_error) = run(
            &link,
            &mut tracker,
            &state,
            "Glob",
            serde_json::json!({"pattern": "**/*"}),
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("notes.txt"), "{out}");
        assert!(
            !out.contains("hidden.txt"),
            "the denied directory must not be listed through a symlinked cwd: {out}"
        );

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&real);
    }

    /// Walking through a directory symlink resolves the root to its real
    /// directory, and the final-target checks still apply to every entry.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_checks_cover_root_symlink_traversal() {
        let dir = temp_dir("symlink-root-traversal");
        let outside = outside_dir(&dir, "symlink-root-outside");
        std::fs::write(outside.join("note.txt"), "PUBLIC-NOTE").unwrap();
        std::fs::write(outside.join(".env"), "ROOT-LINK-SECRET").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("linked")).unwrap();
        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();

        let (out, is_error) = run(
            &dir,
            &mut tracker,
            &state,
            "Read",
            serde_json::json!({"path": "linked/note.txt"}),
        )
        .await;
        assert!(!is_error, "an ordinary linked read stays allowed: {out}");
        assert!(out.contains("PUBLIC-NOTE"), "{out}");

        let (out, is_error) = run(
            &dir,
            &mut tracker,
            &state,
            "Read",
            serde_json::json!({"path": "linked/.env"}),
        )
        .await;
        assert!(is_error, "the linked target is sensitive: {out}");
        assert!(!out.contains("ROOT-LINK-SECRET"), "{out}");

        let (out, is_error) = run(
            &dir,
            &mut tracker,
            &state,
            "Grep",
            serde_json::json!({"pattern": "ROOT-LINK|PUBLIC", "path": "linked"}),
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("PUBLIC-NOTE"), "{out}");
        assert!(!out.contains("ROOT-LINK-SECRET"), "{out}");

        let (out, is_error) = run(
            &dir,
            &mut tracker,
            &state,
            "Glob",
            serde_json::json!({"pattern": "*", "path": "linked"}),
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("note.txt"), "{out}");
        assert!(!out.contains(".env"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// A link outside a credential directory that points into one is still a
    /// credential read: the veto follows the final object, not the link name.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_symlink_into_a_credential_directory_is_refused() {
        let _env = credential_env_lock().await;
        let dir = temp_dir("credential-symlink");
        let profile = dir.join("profile");
        std::fs::create_dir_all(profile.join(".ssh")).unwrap();
        let known_hosts = profile.join(".ssh").join("known_hosts");
        std::fs::write(&known_hosts, "KNOWN-HOSTS-ENTRY").unwrap();
        std::os::unix::fs::symlink(&known_hosts, dir.join("innocent.txt")).unwrap();
        let previous = std::env::var_os("USERPROFILE");
        unsafe { std::env::set_var("USERPROFILE", &profile) };

        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();
        let read = run(
            &dir,
            &mut tracker,
            &state,
            "Read",
            serde_json::json!({"path": "innocent.txt"}),
        )
        .await;
        if let Some(previous) = previous {
            unsafe { std::env::set_var("USERPROFILE", previous) };
        } else {
            unsafe { std::env::remove_var("USERPROFILE") };
        }

        assert!(read.1, "a credential target is refused: {}", read.0);
        assert!(read.0.contains("credential directory"), "{}", read.0);
        assert!(!read.0.contains("KNOWN-HOSTS-ENTRY"), "{}", read.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `USERPROFILE` is process-wide; the credential tests redirect it. An async
    /// lock: the held region spans awaits.
    async fn credential_env_lock() -> tokio::sync::OwnedSemaphorePermit {
        static LOCK: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)))
            .clone()
            .acquire_owned()
            .await
            .expect("credential env lock")
    }

    /// A one-file read approval names the canonical target and lifts both the
    /// sensitive-name and deny-rule vetoes for exactly that file — including
    /// through Grep/Glob, whose per-entry filters share the same lift. Without
    /// this the dialog would grant an object the tool then silently skipped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_one_file_approval_lifts_the_grep_and_glob_veto() {
        let dir = temp_dir("approved-credential-grep");
        let target = dir.join("known_hosts.pem");
        std::fs::write(&target, "APPROVED-SECRET").unwrap();
        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();
        *state.sandbox_project.write().expect("project layer") = project_denies(&["**/*.pem"]);
        let root = target.canonicalize().expect("canonical target");

        let grep = run_with_read_roots(
            &dir,
            &mut tracker,
            &state,
            "Grep",
            serde_json::json!({"pattern": "APPROVED-SECRET", "path": "known_hosts.pem"}),
            vec![root.clone()],
        )
        .await;
        assert!(!grep.1, "{}", grep.0);
        assert!(grep.0.contains("APPROVED-SECRET"), "{}", grep.0);

        let glob = run_with_read_roots(
            &dir,
            &mut tracker,
            &state,
            "Glob",
            serde_json::json!({"pattern": "*.pem", "path": "."}),
            vec![root],
        )
        .await;
        assert!(!glob.1, "{}", glob.0);
        assert!(glob.0.contains("known_hosts.pem"), "{}", glob.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A path the sandbox's deny rules cover is refused by Read, and Grep does
    /// not search it either.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sandbox_denied_paths_are_not_readable() {
        let dir = temp_dir("deny-read");
        std::fs::write(dir.join("secret.pem"), "PEM-SECRET").unwrap();
        std::fs::write(dir.join("readable.txt"), "PUBLIC").unwrap();
        let state = SessionToolState::for_test();
        let mut tracker = ChangeTracker::default();
        // The workspace's own layer, not the process-wide slot: other tests in
        // this binary spawn agents, and an agent installs the global settings at
        // startup (wiping a global set mid-test). The project layer is
        // per-session, which is also the mechanism it exercises.
        *state.sandbox_project.write().expect("project layer") = project_denies(&["**/*.pem"]);

        let read = run(
            &dir,
            &mut tracker,
            &state,
            "Read",
            serde_json::json!({"path": "secret.pem"}),
        )
        .await;
        let grep = run(
            &dir,
            &mut tracker,
            &state,
            "Grep",
            serde_json::json!({"pattern": "PEM-SECRET", "path": "."}),
        )
        .await;
        let glob = run(
            &dir,
            &mut tracker,
            &state,
            "Glob",
            serde_json::json!({"pattern": "*.pem", "path": "."}),
        )
        .await;
        let readable = run(
            &dir,
            &mut tracker,
            &state,
            "Read",
            serde_json::json!({"path": "readable.txt"}),
        )
        .await;

        assert!(read.1, "{}", read.0);
        assert!(read.0.contains("sandbox"), "{}", read.0);
        assert!(!read.0.contains("PEM-SECRET"), "{}", read.0);
        assert!(!grep.0.contains("PEM-SECRET"), "{}", grep.0);
        assert!(!glob.0.contains("secret.pem"), "{}", glob.0);
        assert!(!readable.1, "an unruled file still reads: {}", readable.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
