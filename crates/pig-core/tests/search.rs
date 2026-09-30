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
    assert!(out.contains("敏感 1"), "跳过统计应含敏感文件: {out}");

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_truncates_overlong_matched_lines() {
    let dir = temp_dir("grep-longline");
    // 1200 字符的命中行（模拟 minified 文件）：截断到 500 + 标注；短行原样
    let long_line = format!("match {}", "x".repeat(1200));
    std::fs::write(dir.join("big.js"), format!("{long_line}\nshort match\n")).unwrap();
    let (out, is_error, ..) = run_tool(&dir, "Grep", serde_json::json!({"pattern": "match"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("[...行超长已截断]"), "{out}");
    let long_row = out
        .lines()
        .find(|l| l.contains("行超长已截断"))
        .expect("截断行仍在输出里");
    assert!(
        long_row.chars().count() < 600,
        "截断后仍过长（{} 字符）",
        long_row.chars().count()
    );
    assert!(out.contains("short match"), "短行不受影响: {out}");
    assert!(
        !out.contains(&"x".repeat(600)),
        "超长行未截断: {}",
        &out[..out.chars().count().min(200)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_paginates_by_matched_lines() {
    let dir = temp_dir("grep-page");
    let body: String = (1..=15).map(|i| format!("hit line {i}\n")).collect();
    std::fs::write(dir.join("page.txt"), body).unwrap();

    // 第一页:5 行 + 续读提示
    let (out, is_error, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hit", "head_limit": 5}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(out.lines().filter(|l| l.contains(": ")).count(), 5, "{out}");
    assert!(out.contains("用 offset=5 续读"), "{out}");

    // 中间页
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hit", "head_limit": 5, "offset": 5}),
    )
    .await;
    assert!(out.contains("hit line 6"), "{out}");
    assert!(out.contains("用 offset=10 续读"), "{out}");

    // 末页:自然扫完,显示总数
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hit", "head_limit": 5, "offset": 10}),
    )
    .await;
    assert!(out.contains("hit line 15"), "{out}");
    assert!(out.contains("共 15 行"), "{out}");

    // offset 超出
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hit", "offset": 15}),
    )
    .await;
    assert!(out.contains("超出命中总数 15"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_context_lines_merge_windows() {
    let dir = temp_dir("grep-ctx");
    // 行号:1 filler / 2 hit / 3 filler / 4 filler / 5 hit / 6 filler / 7 filler / 8 filler / 9 hit
    std::fs::write(
        dir.join("ctx.txt"),
        "l1\nhit two\nl3\nl4\nhit five\nl6\nl7\nl8\nhit nine\n",
    )
    .unwrap();

    let (out, is_error, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hit", "context": 1}),
    )
    .await;
    assert!(!is_error, "{out}");
    // 命中 2(窗口 1-3)与命中 5(窗口 4-6)相邻:合并为 1-6;命中 9(窗口 8-9 后无)独立
    assert!(out.contains("ctx.txt:2: hit two"), "{out}");
    assert!(out.contains("ctx.txt:1: l1"), "窗口含命中前行: {out}");
    assert!(out.contains("ctx.txt:6: l6"), "合并窗口到 6: {out}");
    assert!(!out.contains("ctx.txt:7:"), "合并窗口不应到 7: {out}");
    // 恰好一个 -- 分隔(两个窗口)
    assert_eq!(out.lines().filter(|l| *l == "--").count(), 1, "{out}");
    assert!(out.contains("ctx.txt:9: hit nine"), "{out}");

    // 显式 before=0 优先于 context
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hit", "context": 1, "before": 0}),
    )
    .await;
    assert!(!out.contains("ctx.txt:1: l1"), "before=0 不带前行: {out}");
    assert!(out.contains("ctx.txt:3: l3"), "after=1 带后行: {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_files_and_count_modes() {
    let dir = temp_dir("grep-modes");
    std::fs::write(dir.join("a.txt"), "needle here\nneedle again\nplain\n").unwrap();
    std::fs::write(dir.join("b.txt"), "no match\n").unwrap();
    std::fs::write(dir.join("c.txt"), "one needle needle line\n").unwrap(); // 一行两次命中

    let (out, is_error, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "needle", "output_mode": "files_with_matches"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("a.txt") && out.contains("c.txt"), "{out}");
    assert!(!out.contains("b.txt"), "{out}");
    assert!(
        !out.lines().any(|l| l.contains(": ")),
        "files 模式只列路径不带行内容: {out}"
    );

    // count:rg -c 口径——命中行数,一行两次命中算 1
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "needle", "output_mode": "count"}),
    )
    .await;
    assert!(out.contains("a.txt:2"), "{out}");
    assert!(out.contains("c.txt:1"), "一行两次命中算 1: {out}");

    // files 模式分页单位是文件
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "needle", "output_mode": "files_with_matches", "head_limit": 1}),
    )
    .await;
    assert!(
        out.lines().filter(|l| !l.starts_with('[')).count() == 1,
        "head_limit=1 只出一个文件: {out}"
    );
    assert!(out.contains("用 offset=1 续读"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_decodes_gbk_and_utf16_files() {
    let dir = temp_dir("grep-enc");
    // GBK:「中文」= D6 D0 CE C4 + ASCII needle(手写字节,decode 走 GBK round-trip)
    let mut gbk = vec![0xd6, 0xd0, 0xce, 0xc4];
    gbk.extend_from_slice(b" needle tail\n");
    std::fs::write(dir.join("gbk.txt"), gbk).unwrap();
    // UTF-16LE 带 BOM
    let mut utf16 = vec![0xff, 0xfe];
    for unit in "utf16 needle line\n".encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(dir.join("u16.txt"), utf16).unwrap();

    let (out, is_error, ..) =
        run_tool(&dir, "Grep", serde_json::json!({"pattern": "needle"})).await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("gbk.txt:1: 中文 needle tail"),
        "GBK 文件应解码后命中: {out}"
    );
    assert!(out.contains("u16.txt:1: utf16 needle line"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_reports_skipped_files_by_reason() {
    let dir = temp_dir("grep-skip");
    std::fs::write(dir.join(".env"), "needle secret\n").unwrap(); // 敏感
    // 二进制：单侧孤立 NUL（避开 UTF-16 零字节奇偶启发式的模糊区）
    std::fs::write(dir.join("bin.dat"), b"\x00\x01\x02bin needle\n").unwrap();
    let big = dir.join("big.log");
    std::fs::write(&big, b"needle\n").unwrap();
    let f = std::fs::File::options().write(true).open(&big).unwrap();
    f.set_len(3 * 1024 * 1024).unwrap(); // 超过 2MB（set_len 稀疏扩展）
    drop(f);
    std::fs::write(dir.join("ok.txt"), "needle fine\n").unwrap();

    let (out, is_error, ..) =
        run_tool(&dir, "Grep", serde_json::json!({"pattern": "needle"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("ok.txt:1: needle fine"), "{out}");
    assert!(out.contains("敏感 1"), "{out}");
    assert!(out.contains("二进制/未知编码 1"), "{out}");
    assert!(out.contains("超过 2MB 1"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_char_budget_truncates_early() {
    let dir = temp_dir("grep-budget");
    // 100 行 × ~600 字符:截断后每行 ~520,~58 行即超 30k 预算
    let body: String = (1..=100)
        .map(|_| format!("needle{}\n", "x".repeat(600)))
        .collect();
    std::fs::write(dir.join("big.txt"), body).unwrap();

    let (out, is_error, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "needle", "head_limit": 0}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("字符上限"), "{out}");
    assert!(out.contains("下一页 offset="), "{out}");
    let rows = out.lines().filter(|l| l.contains(": ")).count();
    assert!(rows < 100, "应在预算处提前停止(实际 {rows} 行): {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_paginates_with_offset() {
    let dir = temp_dir("glob-page");
    for i in 1..=8 {
        std::fs::write(dir.join(format!("g{i}.txt")), "x\n").unwrap();
    }
    let (out, is_error, ..) = run_tool(
        &dir,
        "Glob",
        serde_json::json!({"pattern": "g*.txt", "head_limit": 3}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(
        out.lines().filter(|l| l.starts_with("g")).count(),
        3,
        "{out}"
    );
    assert!(out.contains("用 offset=3 续读"), "{out}");

    let (out, ..) = run_tool(
        &dir,
        "Glob",
        serde_json::json!({"pattern": "g*.txt", "offset": 6}),
    )
    .await;
    assert_eq!(
        out.lines().filter(|l| l.starts_with("g")).count(),
        2,
        "末页 2 个: {out}"
    );
    assert!(out.contains("共 8 个"), "{out}");
}
