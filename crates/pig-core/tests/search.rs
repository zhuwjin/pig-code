//! Glob/Grep/search_files walk-engine tests (ignore crate: respects
//! .gitignore, includes hidden files, skips VCS directories, sensitive-file
//! filtering, mtime ordering, ignore_case).

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
            fs_grant: None,
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

    // **/*.rs: gitignore excludes target, .git never appears, top-level files also match
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "**/*.rs"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("src/a.rs"), "{out}");
    assert!(
        out.contains("main.rs"),
        "** should cover top-level files: {out}"
    );
    assert!(!out.contains("target/x.rs"), "gitignore exclusion: {out}");
    assert!(
        !out.contains(".git/"),
        "VCS directories never appear: {out}"
    );

    // Hidden directories are visible: .github/workflows can be found
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "**/*.yml"})).await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains(".github/workflows/ci.yml"),
        "hidden files should be included: {out}"
    );

    // A pattern without / matches file names only: nested files still match
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "*.rs"})).await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("src/a.rs"),
        "nested files matched by file name: {out}"
    );
    assert!(out.contains("main.rs"), "{out}");

    // An invalid pattern keeps its error copy
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "["})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("Invalid glob pattern"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_brace_expansion() {
    let dir = temp_dir("glob-brace");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(dir.join("README.md"), "# t\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "x\n").unwrap();

    // Brace alternation: match multiple extensions in one go (the glob crate does not support it natively; done by expanding sub-patterns)
    let (out, is_error, _, _, _) = run_tool(
        &dir,
        "Glob",
        serde_json::json!({"pattern": "**/*.{rs,toml,md}"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("src/a.rs"), "{out}");
    assert!(out.contains("Cargo.toml"), "{out}");
    assert!(out.contains("README.md"), "{out}");
    assert!(!out.contains("notes.txt"), "{out}");

    // A brace pattern without / still matches file names only (nested files match)
    let (out, _, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "*.{toml,md}"})).await;
    assert!(out.contains("Cargo.toml"), "{out}");
    assert!(out.contains("README.md"), "{out}");
    assert!(!out.contains("a.rs"), "{out}");

    // Nested braces
    std::fs::write(dir.join("icon.svg"), "<svg/>\n").unwrap();
    let (out, _, _, _, _) = run_tool(
        &dir,
        "Glob",
        serde_json::json!({"pattern": "**/*.{rs,{svg,toml}}"}),
    )
    .await;
    assert!(out.contains("src/a.rs"), "{out}");
    assert!(out.contains("icon.svg"), "{out}");
    assert!(out.contains("Cargo.toml"), "{out}");
    assert!(!out.contains("README.md"), "{out}");

    // An unclosed { is a parse error (globset UnclosedAlternates): report explicitly instead of silently matching nothing
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "**/*.{rs"})).await;
    assert!(is_error, "{out}");
    assert!(out.contains("Invalid glob pattern"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_star_does_not_cross_separator() {
    let dir = temp_dir("glob-sep");
    std::fs::create_dir_all(dir.join("src/deep")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.join("src/deep/b.rs"), "fn b() {}\n").unwrap();
    std::fs::write(dir.join("c.rs"), "fn c() {}\n").unwrap();

    // gitignore semantics (same as ripgrep --glob/kimi-code): * does not cross /, matches direct children only
    let (out, is_error, _, _, _) =
        run_tool(&dir, "Glob", serde_json::json!({"pattern": "src/*.rs"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("src/a.rs"), "{out}");
    assert!(!out.contains("deep/b.rs"), "* must not cross /: {out}");
    assert!(!out.contains("c.rs"), "{out}");

    // Crossing levels requires **; the **/ prefix can cover zero directories
    let (out, _, _, _, _) = run_tool(&dir, "Glob", serde_json::json!({"pattern": "**/*.rs"})).await;
    assert!(out.contains("src/deep/b.rs"), "{out}");
    assert!(out.contains("c.rs"), "{out}");

    // A pattern without / matches file names at any depth
    let (out, _, _, _, _) = run_tool(&dir, "Glob", serde_json::json!({"pattern": "*.rs"})).await;
    assert!(out.contains("src/deep/b.rs"), "{out}");
    assert!(out.contains("c.rs"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_include_brace_expansion() {
    let dir = temp_dir("grep-include-brace");
    std::fs::write(dir.join("a.rs"), "hello rust\n").unwrap();
    std::fs::write(dir.join("b.toml"), "hello toml\n").unwrap();
    std::fs::write(dir.join("c.txt"), "hello txt\n").unwrap();

    let (out, is_error, _, _, _) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hello", "include": "*.{rs,toml}"}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("a.rs"), "{out}");
    assert!(out.contains("b.toml"), "{out}");
    assert!(!out.contains("c.txt"), "{out}");
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
    let new_pos = out.find("new.rs").expect("new.rs present in results");
    let old_pos = out.find("old.rs").expect("old.rs present in results");
    assert!(
        new_pos < old_pos,
        "mtime descending: most recently modified first: {out}"
    );
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
        ".env should be filtered out: {out}"
    );
    assert!(out.contains("[Filtered out 1 sensitive file(s)]"), "{out}");
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
        "hidden directories should be searchable: {out}"
    );
    assert!(
        !out.contains("target/skip.txt"),
        "gitignore exclusion: {out}"
    );
    assert!(
        !out.contains("TODO=secret"),
        ".env content must not leak: {out}"
    );
    assert!(
        out.contains("sensitive 1"),
        "skip stats should include sensitive files: {out}"
    );

    // Explicitly grepping a single sensitive file is blocked too
    let (out, _, _, _, _) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "TODO", "path": ".env"}),
    )
    .await;
    assert!(
        !out.contains("TODO=secret"),
        "an explicit single sensitive file is also skipped: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_ignore_case() {
    let dir = temp_dir("grep-case");
    std::fs::write(dir.join("hello.txt"), "Hello World\n").unwrap();

    let (out, is_error, _, _, _) =
        run_tool(&dir, "Grep", serde_json::json!({"pattern": "hello"})).await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "(no matches)");

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
        "gitignored files stay out of @ completion: {all:?}"
    );

    let github = tool::search_files(&dir, "github", 50);
    assert!(
        github.iter().any(|p| p == ".github/workflows/ci.yml"),
        "hidden files enter @ completion: {github:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_truncates_overlong_matched_lines() {
    let dir = temp_dir("grep-longline");
    // A 1200-char matched line (simulating a minified file): truncated to 500 + annotation; short lines unchanged
    let long_line = format!("match {}", "x".repeat(1200));
    std::fs::write(dir.join("big.js"), format!("{long_line}\nshort match\n")).unwrap();
    let (out, is_error, ..) = run_tool(&dir, "Grep", serde_json::json!({"pattern": "match"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("[...line too long; truncated]"), "{out}");
    let long_row = out
        .lines()
        .find(|l| l.contains("line too long; truncated"))
        .expect("the truncated line is still in the output");
    assert!(
        long_row.chars().count() < 600,
        "still too long after truncation ({} chars)",
        long_row.chars().count()
    );
    assert!(
        out.contains("short match"),
        "short lines are unaffected: {out}"
    );
    assert!(
        !out.contains(&"x".repeat(600)),
        "overlong line was not truncated: {}",
        &out[..out.chars().count().min(200)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_paginates_by_matched_lines() {
    let dir = temp_dir("grep-page");
    let body: String = (1..=15).map(|i| format!("hit line {i}\n")).collect();
    std::fs::write(dir.join("page.txt"), body).unwrap();

    // First page: 5 lines + continuation hint
    let (out, is_error, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hit", "head_limit": 5}),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(out.lines().filter(|l| l.contains(": ")).count(), 5, "{out}");
    assert!(out.contains("continue with offset=5"), "{out}");

    // Middle page
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hit", "head_limit": 5, "offset": 5}),
    )
    .await;
    assert!(out.contains("hit line 6"), "{out}");
    assert!(out.contains("continue with offset=10"), "{out}");

    // Last page: scanned to the end naturally, shows the total
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hit", "head_limit": 5, "offset": 10}),
    )
    .await;
    assert!(out.contains("hit line 15"), "{out}");
    assert!(out.contains("lines of 15"), "{out}");

    // offset beyond the end
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hit", "offset": 15}),
    )
    .await;
    assert!(out.contains("beyond the total of 15"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_context_lines_merge_windows() {
    let dir = temp_dir("grep-ctx");
    // Line numbers: 1 filler / 2 hit / 3 filler / 4 filler / 5 hit / 6 filler / 7 filler / 8 filler / 9 hit
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
    // Hit 2 (window 1-3) and hit 5 (window 4-6) are adjacent: merged into 1-6; hit 9 (window 8-9, nothing after) is separate
    assert!(out.contains("ctx.txt:2: hit two"), "{out}");
    assert!(
        out.contains("ctx.txt:1: l1"),
        "window includes the line before the hit: {out}"
    );
    assert!(
        out.contains("ctx.txt:6: l6"),
        "merged window reaches line 6: {out}"
    );
    assert!(
        !out.contains("ctx.txt:7:"),
        "merged window should not reach line 7: {out}"
    );
    // Exactly one -- separator (two windows)
    assert_eq!(out.lines().filter(|l| *l == "--").count(), 1, "{out}");
    assert!(out.contains("ctx.txt:9: hit nine"), "{out}");

    // Explicit before=0 takes precedence over context
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "hit", "context": 1, "before": 0}),
    )
    .await;
    assert!(
        !out.contains("ctx.txt:1: l1"),
        "before=0 omits preceding lines: {out}"
    );
    assert!(
        out.contains("ctx.txt:3: l3"),
        "after=1 includes following lines: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_files_and_count_modes() {
    let dir = temp_dir("grep-modes");
    std::fs::write(dir.join("a.txt"), "needle here\nneedle again\nplain\n").unwrap();
    std::fs::write(dir.join("b.txt"), "no match\n").unwrap();
    std::fs::write(dir.join("c.txt"), "one needle needle line\n").unwrap(); // two hits on one line

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
        "files mode lists only paths without line content: {out}"
    );

    // count: rg -c semantics — matched line count, two hits on one line count as 1
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "needle", "output_mode": "count"}),
    )
    .await;
    assert!(out.contains("a.txt:2"), "{out}");
    assert!(
        out.contains("c.txt:1"),
        "two hits on one line count as 1: {out}"
    );

    // In files mode the pagination unit is files
    let (out, ..) = run_tool(
        &dir,
        "Grep",
        serde_json::json!({"pattern": "needle", "output_mode": "files_with_matches", "head_limit": 1}),
    )
    .await;
    assert!(
        out.lines().filter(|l| !l.starts_with('[')).count() == 1,
        "head_limit=1 yields a single file: {out}"
    );
    assert!(out.contains("continue with offset=1"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_decodes_gbk_and_utf16_files() {
    let dir = temp_dir("grep-enc");
    // GBK: "中文" = D6 D0 CE C4 + ASCII needle (hand-written bytes; decode goes through the GBK round-trip)
    let mut gbk = vec![0xd6, 0xd0, 0xce, 0xc4];
    gbk.extend_from_slice(b" needle tail\n");
    std::fs::write(dir.join("gbk.txt"), gbk).unwrap();
    // UTF-16LE with BOM
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
        "GBK files should hit after decoding: {out}"
    );
    assert!(out.contains("u16.txt:1: utf16 needle line"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_reports_skipped_files_by_reason() {
    let dir = temp_dir("grep-skip");
    std::fs::write(dir.join(".env"), "needle secret\n").unwrap(); // sensitive
    // Binary: a lone one-sided NUL (avoids the ambiguous zone of the UTF-16 zero-byte parity heuristic)
    std::fs::write(dir.join("bin.dat"), b"\x00\x01\x02bin needle\n").unwrap();
    let big = dir.join("big.log");
    std::fs::write(&big, b"needle\n").unwrap();
    let f = std::fs::File::options().write(true).open(&big).unwrap();
    f.set_len(3 * 1024 * 1024).unwrap(); // over 2MB (set_len sparse extension)
    drop(f);
    std::fs::write(dir.join("ok.txt"), "needle fine\n").unwrap();

    let (out, is_error, ..) =
        run_tool(&dir, "Grep", serde_json::json!({"pattern": "needle"})).await;
    assert!(!is_error, "{out}");
    assert!(out.contains("ok.txt:1: needle fine"), "{out}");
    assert!(out.contains("sensitive 1"), "{out}");
    assert!(out.contains("binary/undecodable 1"), "{out}");
    assert!(out.contains("over 2MB 1"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_char_budget_truncates_early() {
    let dir = temp_dir("grep-budget");
    // 100 lines × ~600 chars: after truncation each line is ~520, ~58 lines exceed the 30k budget
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
    assert!(out.contains("character budget"), "{out}");
    assert!(out.contains("next page offset="), "{out}");
    let rows = out.lines().filter(|l| l.contains(": ")).count();
    assert!(
        rows < 100,
        "should stop early at the budget (got {rows} rows): {out}"
    );
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
    assert!(out.contains("continue with offset=3"), "{out}");

    let (out, ..) = run_tool(
        &dir,
        "Glob",
        serde_json::json!({"pattern": "g*.txt", "offset": 6}),
    )
    .await;
    assert_eq!(
        out.lines().filter(|l| l.starts_with("g")).count(),
        2,
        "last page has 2 items: {out}"
    );
    assert!(out.contains("of 8"), "{out}");
}
