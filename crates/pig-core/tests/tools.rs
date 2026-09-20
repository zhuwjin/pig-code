//! 工具层单元测试：edit 失败分支、路径逃逸、write/diff/revert。

use pig_core::tool::{self, ChangeTracker, ToolContext};
use pig_core::provider::ToolCall;

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

    let (_, is_error, _) = tool::execute(
        &call("edit", serde_json::json!({"path": "a.txt", "old_string": "baz", "new_string": "x"})),
        ToolContext { cwd: &dir, tracker: &mut tracker },
    )
    .await;
    assert!(is_error);

    let (output, is_error, _) = tool::execute(
        &call("edit", serde_json::json!({"path": "a.txt", "old_string": "foo", "new_string": "x"})),
        ToolContext { cwd: &dir, tracker: &mut tracker },
    )
    .await;
    assert!(is_error);
    assert!(output.contains("2 次"), "应提示多处匹配: {output}");

    let (_, is_error, change) = tool::execute(
        &call("edit", serde_json::json!({"path": "a.txt", "old_string": "bar", "new_string": "x"})),
        ToolContext { cwd: &dir, tracker: &mut tracker },
    )
    .await;
    assert!(!is_error);
    let change = change.expect("edit 应产生 FileChange");
    assert_eq!(change.path, "a.txt");
    assert_eq!((change.additions, change.deletions), (1, 1));
    assert!(change.unified_diff.contains("-bar") && change.unified_diff.contains("+x"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_escape_rejected() {
    let dir = temp_dir("escape");
    let mut tracker = ChangeTracker::default();

    for (tool_name, args) in [
        ("read_file", serde_json::json!({"path": "../outside.txt"})),
        ("write_file", serde_json::json!({"path": "../outside.txt", "content": "x"})),
        ("edit", serde_json::json!({"path": "../outside.txt", "old_string": "a", "new_string": "b"})),
        ("read_file", serde_json::json!({"path": "sub/../../outside.txt"})),
    ] {
        let (output, is_error, _) = tool::execute(
            &call(tool_name, args),
            ToolContext { cwd: &dir, tracker: &mut tracker },
        )
        .await;
        assert!(is_error, "{tool_name} 越界应失败");
        assert!(
            output.contains("越出工作目录") || output.contains("不存在") || output.contains("读取失败"),
            "错误信息: {output}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_diff_revert_cycle() {
    let dir = temp_dir("write");
    let mut tracker = ChangeTracker::default();

    let (_, is_error, change) = tool::execute(
        &call("write_file", serde_json::json!({"path": "sub/new.txt", "content": "a\nb\n"})),
        ToolContext { cwd: &dir, tracker: &mut tracker },
    )
    .await;
    assert!(!is_error);
    let change = change.expect("write 应产生 FileChange");
    assert_eq!(change.path, "sub/new.txt", "路径用正斜杠相对化");
    assert_eq!((change.additions, change.deletions), (2, 0));

    // 第二次修改 diff 仍相对原始快照（不存在 → 全新增）
    let (_, is_error, change) = tool::execute(
        &call("edit", serde_json::json!({"path": "sub/new.txt", "old_string": "b", "new_string": "B"})),
        ToolContext { cwd: &dir, tracker: &mut tracker },
    )
    .await;
    assert!(!is_error);
    let change = change.unwrap();
    assert_eq!((change.additions, change.deletions), (2, 0), "仍是原始→当前: {}", change.unified_diff);

    tracker.revert(&dir.join("sub/new.txt")).unwrap();
    assert!(!dir.join("sub/new.txt").exists(), "新建文件撤销即删除");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_modified_file_restores_content() {
    let dir = temp_dir("revert");
    std::fs::write(dir.join("m.txt"), "original\n").unwrap();
    let mut tracker = ChangeTracker::default();

    tool::execute(
        &call("edit", serde_json::json!({"path": "m.txt", "old_string": "original", "new_string": "changed"})),
        ToolContext { cwd: &dir, tracker: &mut tracker },
    )
    .await;
    assert_eq!(std::fs::read_to_string(dir.join("m.txt")).unwrap(), "changed\n");

    tracker.revert(&dir.join("m.txt")).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("m.txt")).unwrap(), "original\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_and_grep() {
    let dir = temp_dir("search");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "fn main() {}\n// TODO 修复\n").unwrap();
    std::fs::write(dir.join("src/b.md"), "TODO 文档\n").unwrap();
    let mut tracker = ChangeTracker::default();

    let (out, is_error, _) = tool::execute(
        &call("glob", serde_json::json!({"pattern": "**/*.rs"})),
        ToolContext { cwd: &dir, tracker: &mut tracker },
    )
    .await;
    assert!(!is_error);
    assert!(out.contains("src/a.rs") && !out.contains("b.md"), "{out}");

    let (out, is_error, _) = tool::execute(
        &call("grep", serde_json::json!({"pattern": "TODO"})),
        ToolContext { cwd: &dir, tracker: &mut tracker },
    )
    .await;
    assert!(!is_error);
    assert!(out.contains("src/a.rs:2:") && out.contains("src/b.md:1:"), "{out}");

    let (out, _, _) = tool::execute(
        &call("grep", serde_json::json!({"pattern": "TODO", "include": "*.rs"})),
        ToolContext { cwd: &dir, tracker: &mut tracker },
    )
    .await;
    assert!(out.contains("a.rs") && !out.contains("b.md"), "include 过滤: {out}");
}
