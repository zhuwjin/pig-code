//! Tool-layer unit tests: Edit failure branches, path escape, write/diff/revert.

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
    let dir = std::env::temp_dir().join(format!("pig-core-tools-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_not_found_and_not_unique() {
    let dir = temp_dir("edit");
    std::fs::write(dir.join("a.txt"), "foo\nfoo\nbar\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // Pre-write freshness: an existing file must be Read and registered first
    let (_, is_error, _, _, _) = tool::execute(
        &call("Read", serde_json::json!({"path": "a.txt"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);

    let (_, is_error, _, _, _) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "a.txt", "old_string": "baz", "new_string": "x"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(is_error);

    let (output, is_error, _, _, _) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "a.txt", "old_string": "foo", "new_string": "x"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(is_error);
    assert!(
        output.contains("appears 2 times"),
        "should report multiple matches: {output}"
    );

    let (_, is_error, change, _, _) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "a.txt", "old_string": "bar", "new_string": "x"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    let change = change.expect("Edit should produce a FileChange");
    assert_eq!(change.path, "a.txt");
    assert_eq!((change.additions, change.deletions), (1, 1));
    assert!(change.unified_diff.contains("-bar") && change.unified_diff.contains("+x"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_escape_rejected() {
    let dir = temp_dir("escape");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    for (tool_name, args) in [
        ("Read", serde_json::json!({"path": "../outside.txt"})),
        (
            "Write",
            serde_json::json!({"path": "../outside.txt", "content": "x"}),
        ),
        (
            "Edit",
            serde_json::json!({"path": "../outside.txt", "old_string": "a", "new_string": "b"}),
        ),
        ("Read", serde_json::json!({"path": "sub/../../outside.txt"})),
    ] {
        let (output, is_error, _, _, _) = tool::execute(
            &call(tool_name, args),
            ToolContext {
                cwd: &dir,
                tracker: &mut tracker,
                state: &state,
                fs_grant: None,
            },
        )
        .await;
        assert!(
            is_error,
            "{tool_name} escaping the working directory should fail"
        );
        assert!(
            output.contains("Path escapes the working directory")
                || output.contains("does not exist")
                || output.contains("Failed to read"),
            "error message: {output}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_diff_revert_cycle() {
    let dir = temp_dir("write");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (_, is_error, change, _, _) = tool::execute(
        &call(
            "Write",
            serde_json::json!({"path": "sub/new.txt", "content": "a\nb\n"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    let change = change.expect("Write should produce a FileChange");
    assert_eq!(
        change.path, "sub/new.txt",
        "path relativized with forward slashes"
    );
    assert_eq!((change.additions, change.deletions), (2, 0));

    // The second modification's diff is still relative to the original snapshot (nonexistent → all additions)
    let (_, is_error, change, _, _) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "sub/new.txt", "old_string": "b", "new_string": "B"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    let change = change.unwrap();
    assert_eq!(
        (change.additions, change.deletions),
        (2, 0),
        "still original-to-current: {}",
        change.unified_diff
    );

    tracker.revert(&dir.join("sub/new.txt")).unwrap();
    assert!(
        !dir.join("sub/new.txt").exists(),
        "reverting a created file deletes it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_produces_per_edit_diff() {
    let dir = temp_dir("per-edit-diff");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // Write's per-edit diff: nonexistent → all additions
    let (_, is_error, _, edit, _) = tool::execute(
        &call(
            "Write",
            serde_json::json!({"path": "f.txt", "content": "a\nb\nc\n"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    let edit = edit.expect("Write should carry the per-edit diff");
    assert_eq!(edit.path, "f.txt");
    assert_eq!((edit.additions, edit.deletions), (3, 0));
    assert!(
        edit.unified_diff.contains("+a"),
        "diff content: {}",
        edit.unified_diff
    );

    // Edit's per-edit diff reflects only this one replacement (1 addition 1
    // deletion), distinct from the session-cumulative file_change (relative to
    // the original snapshot)
    let (_, is_error, change, edit, _) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "f.txt", "old_string": "b", "new_string": "B"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    let edit = edit.expect("Edit should carry the per-edit diff");
    assert_eq!(
        (edit.additions, edit.deletions),
        (1, 1),
        "only this replacement: {}",
        edit.unified_diff
    );
    assert!(edit.unified_diff.contains("-b") && edit.unified_diff.contains("+B"));
    let change = change.expect("cumulative diff still present");
    assert_eq!(
        (change.additions, change.deletions),
        (3, 0),
        "cumulative view unchanged (nonexistent original to current)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_changes_are_per_turn_not_cumulative() {
    let dir = temp_dir("turn-changes");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // "Turn one": Write 3 lines: this turn's net delta = all additions
    let (_, is_error, _, _, _) = tool::execute(
        &call(
            "Write",
            serde_json::json!({"path": "f.txt", "content": "a\nb\nc\n"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    let changes = tracker.take_turn_changes(&dir);
    assert_eq!(changes.len(), 1);
    assert_eq!((changes[0].additions, changes[0].deletions), (3, 0));
    assert!(
        tracker.take_turn_changes(&dir).is_empty(),
        "take should clear the state"
    );

    // "Turn two": Edit 1 line: counts this turn only (1 addition 1 deletion), not the session-cumulative view
    let (_, is_error, _, _, _) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "f.txt", "old_string": "b", "new_string": "B"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    let changes = tracker.take_turn_changes(&dir);
    assert_eq!(changes.len(), 1);
    assert_eq!((changes[0].additions, changes[0].deletions), (1, 1));

    // "Turn three": reverted within the same turn: the turn's start and end content match, net delta zero, nothing emitted
    let (_, is_error, _, _, _) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "f.txt", "old_string": "B", "new_string": "b"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    let (_, is_error, _, _, _) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "f.txt", "old_string": "b", "new_string": "B"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    assert!(
        tracker.take_turn_changes(&dir).is_empty(),
        "restoring content within a turn should yield zero net change"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_modified_file_restores_content() {
    let dir = temp_dir("revert");
    std::fs::write(dir.join("m.txt"), "original\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // Pre-write freshness: Read before Edit
    let (_, is_error, _, _, _) = tool::execute(
        &call("Read", serde_json::json!({"path": "m.txt"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);

    tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "m.txt", "old_string": "original", "new_string": "changed"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(dir.join("m.txt")).unwrap(),
        "changed\n"
    );

    tracker.revert(&dir.join("m.txt")).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("m.txt")).unwrap(),
        "original\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_and_grep() {
    let dir = temp_dir("search");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "fn main() {}\n// TODO fix it\n").unwrap();
    std::fs::write(dir.join("src/b.md"), "TODO docs\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error, _, _, _) = tool::execute(
        &call("Glob", serde_json::json!({"pattern": "**/*.rs"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    assert!(out.contains("src/a.rs") && !out.contains("b.md"), "{out}");

    let (out, is_error, _, _, _) = tool::execute(
        &call("Grep", serde_json::json!({"pattern": "TODO"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    assert!(
        out.contains("src/a.rs:2:") && out.contains("src/b.md:1:"),
        "{out}"
    );

    let (out, _, _, _, _) = tool::execute(
        &call(
            "Grep",
            serde_json::json!({"pattern": "TODO", "include": "*.rs"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(
        out.contains("a.rs") && !out.contains("b.md"),
        "include filter: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn todo_list_read_write_replace() {
    let dir = temp_dir("todo");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // Empty read
    let (out, is_error, _, _, _) = tool::execute(
        &call("TodoList", serde_json::json!({})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    assert_eq!(out, "The todo list is empty");

    // Write (state hangs off the ctx handle and persists across calls)
    let (out, is_error, _, _, _) = tool::execute(
        &call(
            "TodoList",
            serde_json::json!({"todos": [
                {"content": "Read the code", "status": "done"},
                {"content": "Change the implementation", "status": "in_progress"},
                {"content": "Run the tests", "status": "pending"},
            ]}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    assert!(out.contains("1. [done] Read the code"), "{out}");
    assert!(
        out.contains("2. [in_progress] Change the implementation"),
        "{out}"
    );

    // Read back
    let (out, is_error, _, _, _) = tool::execute(
        &call("TodoList", serde_json::json!({})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    assert!(out.contains("3. [pending] Run the tests"), "{out}");

    // Full replacement: old items should all disappear
    let (out, _, _, _, _) = tool::execute(
        &call(
            "TodoList",
            serde_json::json!({"todos": [{"content": "Wrap up", "status": "pending"}]}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(out.contains("1. [pending] Wrap up"), "{out}");
    assert!(
        !out.contains("Read the code"),
        "old items should disappear after full replacement: {out}"
    );

    // An invalid status errors, and the list keeps its pre-replacement content
    let (out, is_error, _, _, _) = tool::execute(
        &call(
            "TodoList",
            serde_json::json!({"todos": [{"content": "x", "status": "doing"}]}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(is_error, "invalid status should error: {out}");
    let (out, _, _, _, _) = tool::execute(
        &call("TodoList", serde_json::json!({})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(
        out.contains("1. [pending] Wrap up"),
        "list should stay unchanged after a failed write: {out}"
    );
}

#[test]
fn fetch_url_extract_text_strips_non_content() {
    let html = "<html><head><style>body{color:red}</style><script>var x=1;</script></head>\
        <body><nav>menu</nav><main><h1>Title</h1><p>First paragraph</p><p>Second paragraph</p>\
        <script>ignore()</script><noscript>fallback</noscript><svg><text>icon</text></svg>\
        </main><footer>page footer</footer></body></html>";
    let text = tool::extract_text(html);
    assert!(text.contains("Title"), "{text}");
    assert!(text.contains("First paragraph"), "{text}");
    assert!(text.contains("Second paragraph"), "{text}");
    assert!(
        !text.contains("var x"),
        "head script should be stripped: {text}"
    );
    assert!(
        !text.contains("color:red"),
        "style should be stripped: {text}"
    );
    assert!(
        !text.contains("ignore()"),
        "body script should be stripped: {text}"
    );
    assert!(
        !text.contains("fallback"),
        "noscript should be stripped: {text}"
    );
    assert!(!text.contains("icon"), "svg should be stripped: {text}");
    assert!(
        !text.contains("menu"),
        "main preferred; nav should not appear: {text}"
    );
    assert!(
        !text.contains("page footer"),
        "main preferred; footer should not appear: {text}"
    );
    assert!(
        text.contains("Title\nFirst paragraph"),
        "block-level elements separated by newlines: {text:?}"
    );
}

#[test]
fn fetch_url_extract_text_body_fallback_and_blank_collapse() {
    let html = "<html><body><div><p>A</p></div><div><p>B</p>\n\n\n<p>C</p></div></body></html>";
    let text = tool::extract_text(html);
    assert_eq!(
        text, "A\nB\nC",
        "consecutive blank lines should collapse: {text:?}"
    );
}

#[test]
fn fetch_url_is_private_host_ranges() {
    for host in [
        "localhost",
        "LOCALHOST",
        "localhost.",
        "127.0.0.1",
        "127.5.5.5",
        "::1",
        "[::1]",
        "0.0.0.0",
        "10.0.0.1",
        "10.255.255.255",
        "192.168.1.1",
        "172.16.0.1",
        "172.31.255.1",
        "169.254.1.1",
    ] {
        assert!(
            tool::is_private_host(host),
            "{host} should be classified as private"
        );
    }
    for host in [
        "example.com",
        "8.8.8.8",
        "1.1.1.1",
        "172.15.0.1",
        "172.32.0.1",
        "11.0.0.1",
        "192.167.1.1",
        "10x.example.com",
    ] {
        assert!(!tool::is_private_host(host), "{host} should be allowed");
    }
}

/// Bash run_in_background full lifecycle: start → visible in the registry →
/// Exited(0) → TaskOutput contains the output; the sleep background task
/// TaskStop → Killed (a repeated stop errors); TaskList renders both ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_bash_task_lifecycle() {
    let dir = temp_dir("bgtask");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let task_id_of = |out: &str| {
        out.strip_prefix("Started in the background, task_id: ")
            .and_then(|rest| rest.split('.').next())
            .expect("return message should contain task_id")
            .to_string()
    };

    // Background echo: returns task_id immediately
    let (out, is_error, _, _, _) = tool::execute(
        &call(
            "Bash",
            serde_json::json!({"command": "echo bg-marker", "run_in_background": true}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error, "{out}");
    let task1 = task_id_of(&out);

    // Poll the registry until Exited(0) (with a timeout)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let done = {
            let tasks = state.tasks.lock().expect("tasks lock");
            matches!(
                tasks.iter().find(|t| t.id == task1).map(|t| &t.status),
                Some(pig_protocol::TaskStatus::Exited(0))
            )
        };
        if done {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the background task to exit"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // TaskOutput contains the output
    let (out, is_error, _, _, _) = tool::execute(
        &call("TaskOutput", serde_json::json!({"task_id": task1})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("bg-marker"), "{out}");

    // sleep 30 started in the background → TaskStop → Killed
    let (out, is_error, _, _, _) = tool::execute(
        &call(
            "Bash",
            serde_json::json!({"command": "sleep 30", "run_in_background": true}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error, "{out}");
    let task2 = task_id_of(&out);
    let (out, is_error, _, _, _) = tool::execute(
        &call("TaskStop", serde_json::json!({"task_id": task2})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error, "{out}");
    {
        let tasks = state.tasks.lock().expect("tasks lock");
        let entry = tasks.iter().find(|t| t.id == task2).expect("task2 exists");
        assert!(
            matches!(entry.status, pig_protocol::TaskStatus::Killed),
            "expected Killed: {:?}",
            entry.status
        );
        assert!(entry.ended_at.is_some());
    }
    // Repeated stop → "task already ended" error
    let (_, is_error, _, _, _) = tool::execute(
        &call("TaskStop", serde_json::json!({"task_id": task2})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(is_error, "stopping an already-stopped task should error");

    // TaskList rendering contains both task_ids
    let (out, is_error, _, _, _) = tool::execute(
        &call("TaskList", serde_json::json!({})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains(&task1) && out.contains(&task2), "{out}");
}

#[test]
fn parse_questions_validates_shape() {
    // Normal: one single-select question (header/description) + one multi-select question
    let questions = tool::parse_questions(&serde_json::json!({"questions": [
        {"question": "Pick a plan", "header": "Plan", "options": [{"label": "A"}, {"label": "B", "description": "Alternative"}]},
        {"question": "Pick a scope", "multi_select": true, "options": [{"label": "x"}, {"label": "y"}, {"label": "z"}]},
    ]}))
    .expect("valid arguments");
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0].question, "Pick a plan");
    assert_eq!(questions[0].header.as_deref(), Some("Plan"));
    assert!(!questions[0].multi_select);
    assert_eq!(questions[0].options.len(), 2);
    assert_eq!(
        questions[0].options[1].description.as_deref(),
        Some("Alternative")
    );
    assert!(questions[1].multi_select);
    assert_eq!(questions[1].options.len(), 3);

    // Question count out of bounds: 0 / 5 questions
    assert!(tool::parse_questions(&serde_json::json!({"questions": []})).is_err());
    let five = serde_json::json!({"questions": (0..5)
        .map(|i| serde_json::json!({"question": format!("q{i}"), "options": [{"label": "a"}, {"label": "b"}]}))
        .collect::<Vec<_>>()});
    assert!(tool::parse_questions(&five).is_err());

    // Option count out of bounds: 1 / 5 options
    assert!(
        tool::parse_questions(
            &serde_json::json!({"questions": [{"question": "q", "options": [{"label": "a"}]}]})
        )
        .is_err()
    );
    assert!(
        tool::parse_questions(
            &serde_json::json!({"questions": [{"question": "q", "options": [
                {"label": "1"}, {"label": "2"}, {"label": "3"}, {"label": "4"}, {"label": "5"}
            ]}]})
        )
        .is_err()
    );

    // Empty label / empty question / missing options / missing questions
    assert!(tool::parse_questions(&serde_json::json!({"questions": [{"question": "q", "options": [{"label": " "}, {"label": "b"}]}]})).is_err());
    assert!(tool::parse_questions(&serde_json::json!({"questions": [{"question": " ", "options": [{"label": "a"}, {"label": "b"}]}]})).is_err());
    assert!(tool::parse_questions(&serde_json::json!({"questions": [{"question": "q"}]})).is_err());
    assert!(tool::parse_questions(&serde_json::json!({})).is_err());
}

// ---------- 5.2 FetchURL DNS rebinding protection ----------

#[test]
fn fetch_url_is_private_host_extended_ranges() {
    for host in [
        "100.64.5.5",     // CGNAT 100.64/10
        "198.18.0.1",     // benchmark 198.18/15
        "198.19.255.255", //
        "224.0.0.1",      // multicast
        "fc00::1",        // v6 unique local
        "fd12::1",        //
        "fe80::1",        // v6 link-local
        "foo.localhost",  // localhost subdomain
        "internal",       // single-label hostname (intranet short name)
        "192.0.2.1",      // documentation range TEST-NET-1
    ] {
        assert!(
            tool::is_private_host(host),
            "{host} should be classified as private/reserved"
        );
    }
    for host in [
        "100.63.0.1",
        "198.17.0.1",
        "198.20.0.1",
        "example.com",
        "a.b.internal",
    ] {
        assert!(!tool::is_private_host(host), "{host} should be allowed");
    }
}

#[test]
fn fetch_url_rejects_embedded_credentials() {
    let url = reqwest::Url::parse("http://user:pass@example.com/").unwrap();
    let err = tool::check_fetch_url(&url).unwrap_err();
    assert!(err.contains("must not embed credentials"), "{err}");

    let url = reqwest::Url::parse("http://user@example.com/").unwrap();
    assert!(
        tool::check_fetch_url(&url).is_err(),
        "username-only credentials are also rejected"
    );

    let url = reqwest::Url::parse("http://example.com/").unwrap();
    assert!(tool::check_fetch_url(&url).is_ok());

    let url = reqwest::Url::parse("file:///etc/passwd").unwrap();
    assert!(tool::check_fetch_url(&url).is_err(), "scheme allowlist");
}

/// Pre-read size guard: set_len creates a 101MB logical file (NTFS sparse
/// extension, takes seconds); Read should reject before reading from disk;
/// the guard runs ahead of the freshness check (Edit reports the size error
/// without a prior Read).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_and_edit_reject_oversized_files() {
    let dir = temp_dir("size-cap");

    let huge_read = dir.join("huge_read.log");
    let f = std::fs::File::create(&huge_read).unwrap();
    f.set_len(101 * 1024 * 1024).unwrap();
    drop(f);
    let (out, is_error, ..) = tool::execute(
        &call("Read", serde_json::json!({"path": "huge_read.log"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut ChangeTracker::default(),
            state: &SessionToolState::for_test(),
            fs_grant: None,
        },
    )
    .await;
    assert!(is_error, "should be rejected: {out}");
    assert!(
        out.contains("File too large") && out.contains("100 MB"),
        "message should carry the cap and bypass guidance: {out}"
    );

    // 51MB: below the Read cap but above the Edit cap; even unread, the size error should come first (guard runs first)
    let huge_edit = dir.join("huge_edit.txt");
    let f = std::fs::File::create(&huge_edit).unwrap();
    f.set_len(51 * 1024 * 1024).unwrap();
    drop(f);
    let (out, is_error, ..) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "huge_edit.txt", "old_string": "a", "new_string": "b"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut ChangeTracker::default(),
            state: &SessionToolState::for_test(),
            fs_grant: None,
        },
    )
    .await;
    assert!(is_error, "should be rejected: {out}");
    assert!(
        out.contains("50 MB"),
        "size guard should run before the freshness check: {out}"
    );

    // Overwriting an existing oversized file in full is also rejected (Write's memory guard)
    let (out, is_error, ..) = tool::execute(
        &call(
            "Write",
            serde_json::json!({"path": "huge_read.log", "content": "x"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut ChangeTracker::default(),
            state: &SessionToolState::for_test(),
            fs_grant: None,
        },
    )
    .await;
    assert!(is_error, "should be rejected: {out}");
    assert!(out.contains("File too large"), "{out}");

    let _ = std::fs::remove_file(&huge_read);
    let _ = std::fs::remove_file(&huge_edit);
}

/// Edit tier-4 tolerance: when old_string is written as literal escape
/// sequences like \n, they are auto-unescaped before matching; new_string is
/// unescaped in step; exact hits take priority; unrecognized escapes are not
/// unescaped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_unescape_tier_matches_literal_escapes() {
    let dir = temp_dir("edit-unescape");
    std::fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    // Read first (freshness)
    let (..) = tool::execute(
        &call("Read", serde_json::json!({"path": "a.txt"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;

    // old_string is the literal "alpha\nbeta" (in Rust source \n = backslash+n, two chars)
    let (out, is_error, ..) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "a.txt", "old_string": "alpha\\nbeta", "new_string": "X\\tY"}),
        ),
        ToolContext { cwd: &dir, tracker: &mut tracker, state: &state, fs_grant: None },
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("literal escape sequences unescaped"),
        "should note the tolerance tier: {out}"
    );
    let after = std::fs::read_to_string(dir.join("a.txt")).unwrap();
    assert_eq!(
        after, "X\tY\n",
        "literal \\t in new_string should become a real tab"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_unescape_tier_not_applied_when_exact_or_unknown() {
    let dir = temp_dir("edit-unescape2");
    // The file contains exactly the literal backslash-n two characters
    std::fs::write(dir.join("b.txt"), "a\\nb plain\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    let (..) = tool::execute(
        &call("Read", serde_json::json!({"path": "b.txt"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;

    // Exact hit: no tolerance needed; after replacement no tier note is present
    let (out, is_error, ..) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "b.txt", "old_string": "a\\nb", "new_string": "ok"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(
        !out.contains("tolerant match"),
        "exact hit should not use tolerant matching: {out}"
    );

    // Unrecognized escape (\d is not unescapable): the unescape tier does not apply → NotFound
    std::fs::write(dir.join("c.txt"), "hello\n").unwrap();
    let (..) = tool::execute(
        &call("Read", serde_json::json!({"path": "c.txt"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    let (out, is_error, ..) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "c.txt", "old_string": "hel\\dlo", "new_string": "x"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(is_error, "unrecognized escape should not match: {out}");
    assert!(out.contains("old_string not found"), "{out}");
}

/// Read identical-reread short circuit: same arguments + unchanged content → "file unchanged"; after external modification, full output resumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_shortcircuits_identical_view() {
    let dir = temp_dir("read-unchanged");
    std::fs::write(dir.join("u.txt"), "same content\n").unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (out, is_error, ..) = tool::execute(
        &call("Read", serde_json::json!({"path": "u.txt"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("same content"), "{out}");

    // Reread with same arguments → short circuit
    let (out, is_error, ..) = tool::execute(
        &call("Read", serde_json::json!({"path": "u.txt"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("File unchanged"), "{out}");

    // Different arguments (limit) → normal output
    let (out, ..) = tool::execute(
        &call("Read", serde_json::json!({"path": "u.txt", "limit": 5})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(
        out.contains("same content"),
        "different view parameters should produce full output: {out}"
    );

    // External modification → hash changes, normal output resumes (and the state updates)
    std::fs::write(dir.join("u.txt"), "changed content\n").unwrap();
    let (out, ..) = tool::execute(
        &call("Read", serde_json::json!({"path": "u.txt"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(out.contains("changed content"), "{out}");
}

/// Batch 6: local http allowed + GBK pages decoded per charset (end-to-end: a local HTTP server).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_url_allows_local_http_and_decodes_gbk() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf); // request line (discarded)
        let mut body = b"<html><body>".to_vec();
        body.extend_from_slice(&[0xd6, 0xd0, 0xce, 0xc4]); // GBK "中文"
        body.extend_from_slice(b" local page</body></html>");
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=gbk\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
    });
    let dir = temp_dir("fetch-local");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    let (out, is_error, ..) = tool::execute(
        &call(
            "FetchURL",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/")}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    handle.join().unwrap();
    assert!(!is_error, "{out}");
    assert!(
        out.contains("中文 local page"),
        "GBK body should be decoded: {out}"
    );
}

/// Local/LAN URL validation passes directly (private networks are no longer blocked).
#[test]
fn fetch_url_allows_private_hosts() {
    for url in [
        "http://localhost:8080/",
        "http://127.0.0.1:3000/api",
        "http://192.168.1.1/",
        "http://10.0.0.5:9090/health",
        "http://[::1]:8080/",
    ] {
        let url = reqwest::Url::parse(url).unwrap();
        assert!(
            tool::check_fetch_url(&url).is_ok(),
            "{url} should be allowed"
        );
    }
}

/// Write/Edit atomic write: content correct and no .tmp leftovers in the directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_and_edit_leave_no_temp_files() {
    let dir = temp_dir("atomic-write");
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();

    let (_, is_error, ..) = tool::execute(
        &call(
            "Write",
            serde_json::json!({"path": "a.txt", "content": "hello\nworld\n"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    let (_, is_error, ..) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "a.txt", "old_string": "world", "new_string": "pig"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(!is_error);
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "hello\npig\n"
    );
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temp file leftovers expected: {leftovers:?}"
    );
}

/// Edit multiple-match error carries line numbers (at most 5, "among others" beyond that).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_not_unique_reports_line_numbers() {
    let dir = temp_dir("edit-lines");
    // 6 dup occurrences: lines 1/3/5/7/9/11
    let body: String = (0..6)
        .map(|i| format!("{}\ndup\ntail{i}\n", "head"))
        .collect();
    std::fs::write(dir.join("dup.txt"), body).unwrap();
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    let (..) = tool::execute(
        &call("Read", serde_json::json!({"path": "dup.txt"})),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    let (out, is_error, ..) = tool::execute(
        &call(
            "Edit",
            serde_json::json!({"path": "dup.txt", "old_string": "dup", "new_string": "x"}),
        ),
        ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
        },
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("appears 6 times"), "{out}");
    assert!(
        out.contains("lines 2, 5, 8, 11, 14"),
        "should carry the first few line numbers: {out}"
    );
    assert!(
        out.contains("among others"),
        "more than 5 occurrences should say 'among others': {out}"
    );
}
