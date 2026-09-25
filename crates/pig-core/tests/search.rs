//! Glob/Grep/search_files 遍历引擎测试（ignore crate：尊重 .gitignore、
//! 包含隐藏文件、跳过 VCS 目录、敏感文件过滤、mtime 排序、ignore_case）。

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
    let dir = std::env::temp_dir().join(format!("pig-core-search-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

async fn run_tool(
    dir: &std::path::Path,
    name: &str,
    args: serde_json::Value,
) -> (
    String,
    bool,
    Option<tool::FileChange>,
    Option<pig_protocol::EditDiff>,
    Vec<tool::ToolImage>,
) {
    let mut tracker = ChangeTracker::default();
    let state = SessionToolState::for_test();
    tool::execute(
        &call(name, args),
        ToolContext {
            cwd: dir,
            tracker: &mut tracker,
            state: &state,
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_respects_gitignore_includes_hidden_skips_vcs() {
    let dir = temp_dir("glob-walk");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.join("target/x.rs"), "built\n").unwrap();
    std::fs::write(dir.join(".github/workflows/ci.yml"), "on: push\n").unwrap();
    std::fs::write(dir.join(".git/hooks/pre-commit.rs"), "vcs internals\n").unwrap();
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();

    // **/*.rs：gitignore 排除 target，.git 永不出现，顶层文件也命中
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "**/*.rs"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("src/a.rs"), "{out}");
    assert!(out.contains("main.rs"), "** 应覆盖顶层文件: {out}");
    assert!(!out.contains("target/x.rs"), "gitignore 排除: {out}");
    assert!(!out.contains(".git/"), "VCS 目录永不出现: {out}");

    // 隐藏目录可见：.github/workflows 能被找到
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "**/*.yml"})).await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains(".github/workflows/ci.yml"),
        "隐藏文件应包含: {out}"
    );

    // 不含 / 的 pattern 只比文件名：嵌套文件照样命中
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "*.rs"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("src/a.rs"), "按文件名匹配嵌套文件: {out}");
    assert!(out.contains("main.rs"), "{out}");

    // 非法 pattern 报错文案保留
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "["})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("无效 glob 模式"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_sorts_by_mtime_desc() {
    let dir = temp_dir("glob-mtime");
    std::fs::write(dir.join("old.rs"), "old\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    std::fs::write(dir.join("new.rs"), "new\n").unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "*.rs"})).await;
    assert!(!is_error, "{out}");
    let new_pos = out.find("new.rs").expect("new.rs 在结果中");
    let old_pos = out.find("old.rs").expect("old.rs 在结果中");
    assert!(new_pos < old_pos, "mtime 降序：最近修改的排前面: {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_filters_sensitive_files() {
    let dir = temp_dir("glob-sensitive");
    std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(dir.join("config.toml"), "x = 1\n").unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "*"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("config.toml"), "{out}");
    assert!(
        !out.lines().any(|l| l.trim() == ".env"),
        ".env 应被过滤: {out}"
    );
    assert!(out.contains("[已过滤 1 个敏感文件]"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_hidden_gitignore_and_sensitive() {
    let dir = temp_dir("grep-walk");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::create_dir_all(dir.join(".github")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "// TODO visible\n").unwrap();
    std::fs::write(dir.join("target/skip.txt"), "TODO ignored\n").unwrap();
    std::fs::write(dir.join(".github/config.txt"), "TODO hidden\n").unwrap();
    std::fs::write(dir.join(".env"), "TODO=secret\n").unwrap();
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Grep", serde_json::json!({"pattern": "TODO"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("src/main.rs:1:"), "{out}");
    assert!(
        out.contains(".github/config.txt:1:"),
        "隐藏目录应可搜: {out}"
    );
    assert!(!out.contains("target/skip.txt"), "gitignore 排除: {out}");
    assert!(!out.contains("TODO=secret"), ".env 内容不泄露: {out}");
    assert!(out.contains("[已跳过 1 个敏感文件]"), "{out}");

    // 显式 Grep 单个敏感文件同样被拦
    let (out, _, _, _, _) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "TODO", "path": ".env"}),
    )
    .await;
    assert!(!out.contains("TODO=secret"), "单文件敏感同样跳过: {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_ignore_case() {
    let dir = temp_dir("grep-case");
    std::fs::write(dir.join("hello.txt"), "Hello World\n").unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Grep", serde_json::json!({"pattern": "hello"})).await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "（无匹配内容）");

    let (out, is_error, _, _, _) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hello", "ignore_case": true}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("hello.txt:1: Hello World"), "{out}");
}

#[test]
fn search_files_respects_gitignore_includes_hidden() {
    let dir = temp_dir("at-completion");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(dir.join("src/kept.rs"), "k\n").unwrap();
    std::fs::write(dir.join("target/ignored.rs"), "i\n").unwrap();
    std::fs::write(dir.join(".github/workflows/ci.yml"), "on: push\n").unwrap();
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();

    let all = tool::search_files(&dir, "", 50);
    assert!(all.iter().any(|p| p == "src/kept.rs"), "{all:?}");
    assert!(
        !all.iter().any(|p| p.contains("target/")),
        "gitignore 文件不进 @ 补全: {all:?}"
    );

    let github = tool::search_files(&dir, "github", 50);
    assert!(
        github.iter().any(|p| p == ".github/workflows/ci.yml"),
        "隐藏文件应进 @ 补全: {github:?}"
    );
}
